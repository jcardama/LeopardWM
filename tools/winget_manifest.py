#!/usr/bin/env python3
"""Extract komac dry-run manifests and prepare the installer for winget-pkgs."""

import argparse
import re
import sys
from pathlib import Path

ROOT_KEYS = """PackageIdentifier PackageVersion Channel InstallerLocale Platform MinimumOSVersion
InstallerType NestedInstallerType NestedInstallerFiles Scope InstallModes InstallerSwitches
InstallerSuccessCodes ExpectedReturnCodes UpgradeBehavior Commands Protocols FileExtensions
Dependencies PackageFamilyName ProductCode Capabilities RestrictedCapabilities Markets
InstallerAbortsTerminal ReleaseDate InstallLocationRequired RequireExplicitUpgrade
DisplayInstallWarnings UnsupportedOSArchitectures UnsupportedArguments AppsAndFeaturesEntries
ElevationRequirement InstallationMetadata DownloadCommandProhibited RepairBehavior
ArchiveBinariesDependOnPath Authentication Installers ManifestType ManifestVersion""".split()
KEY = re.compile(rb"^([A-Za-z][A-Za-z0-9]*):(?: (.*))?\r\n$")
FIELD = re.compile(rb"^([A-Za-z][A-Za-z0-9]*): ([^\r\n]+)\r\n", re.MULTILINE)
SAFE_NAME = re.compile(rb"[A-Za-z0-9][A-Za-z0-9.-]*\Z")


def fail(message):
    raise ValueError(message)


def scalar(value):
    if not value:
        fail("empty scalar")
    if value[:1] == b"'" and value[-1:] == b"'":
        return value[1:-1].replace(b"''", b"'")
    if value[:1] == b'"' and value[-1:] == b'"':
        inner = value[1:-1]
        if b"\\" in inner or b'"' in inner:
            fail("unsupported quoted scalar")
        return inner
    if any(c in value for c in (b"'", b'"', b"#", b"[", b"]", b"{", b"}")):
        fail("unsupported scalar")
    return value


def field(data, name):
    values = [match.group(2) for match in FIELD.finditer(data) if match.group(1) == name]
    if len(values) != 1:
        fail(f"expected exactly one {name.decode()} field")
    return scalar(values[0])


def root_blocks(lines):
    starts = []
    for index, line in enumerate(lines):
        match = KEY.fullmatch(line)
        if match:
            name = match.group(1).decode("ascii")
            if name not in ROOT_KEYS:
                fail(f"unknown installer root key {name}")
            starts.append((index, name))
        elif line and line[:1] not in (b" ", b"-", b"#", b"\r"):
            fail("unrecognized installer root line")
    names = [name for _, name in starts]
    if len(names) != len(set(names)) or names != sorted(names, key=ROOT_KEYS.index):
        fail("duplicate or out-of-order installer root keys")
    return [(name, lines[start:(starts[i + 1][0] if i + 1 < len(starts) else len(lines))])
            for i, (start, name) in enumerate(starts)]


def clean_entries(block, indent, version):
    if block[0] != indent + b"AppsAndFeaturesEntries:\r\n":
        fail("unrecognized AppsAndFeaturesEntries header")
    result = [block[0]]
    entries = []
    for line in block[1:]:
        if line.startswith(indent + b"- "):
            entries.append([line])
        elif entries and line.startswith(indent + b"  "):
            entries[-1].append(line)
        else:
            fail("unrecognized AppsAndFeaturesEntries entry")
    if not entries:
        fail("empty AppsAndFeaturesEntries")
    for entry in entries:
        parsed = []
        for i, line in enumerate(entry):
            prefix = indent + (b"- " if i == 0 else b"  ")
            match = re.fullmatch(rb"([A-Za-z][A-Za-z0-9]*): (.+)\r\n", line[len(prefix):])
            if not line.startswith(prefix) or not match:
                fail("unrecognized AppsAndFeaturesEntries field")
            parsed.append(match.group(1))
            if match.group(1) == b"DisplayVersion" and scalar(match.group(2)) == version:
                entry[i] = None
        if len(parsed) != len(set(parsed)):
            fail("duplicate AppsAndFeaturesEntries field")
        entry = [line for line in entry if line is not None]
        if not entry:
            fail("AppsAndFeaturesEntries entry became empty")
        if not entry[0].startswith(indent + b"- "):
            entry[0] = indent + b"- " + entry[0][len(indent) + 2:]
        result.extend(entry)
    return result


def installer(data, version):
    lines = data.splitlines(keepends=True)
    blocks = root_blocks(lines)
    prefix = lines[:next(i for i, line in enumerate(lines) if KEY.fullmatch(line))]
    root = dict(blocks)
    if "Installers" not in root:
        fail("missing Installers")
    for name in ("ProductCode", "AppsAndFeaturesEntries"):
        if name in root and name == "AppsAndFeaturesEntries":
            root[name] = clean_entries(root[name], b"", version)
    raw = root["Installers"]
    if raw[0] != b"Installers:\r\n":
        fail("unrecognized Installers header")
    entries = []
    for line in raw[1:]:
        if line.startswith(b"- Architecture: "):
            entries.append([line])
        elif entries and line.startswith(b"  "):
            entries[-1].append(line)
        else:
            fail("unrecognized installer entry")
    x64 = [entry for entry in entries if entry[0] == b"- Architecture: x64\r\n"]
    if len(x64) != 1 or len(entries) < 2:
        fail("expected one x64 and at least one other installer")
    extracted = {}
    for entry in entries:
        starts = [i for i, line in enumerate(entry) if re.match(rb"^  [A-Za-z][A-Za-z0-9]*:", line)]
        if not starts or starts[0] != 1:
            fail("unrecognized installer fields")
        names = [entry[i].split(b":", 1)[0].strip().decode("ascii") for i in starts]
        if len(names) != len(set(names)):
            fail("duplicate installer field")
        fields = [(name, entry[start:(starts[i + 1] if i + 1 < len(starts) else len(entry))])
                  for i, (start, name) in enumerate(zip(starts, names))]
        rebuilt = [entry[0]]
        for name, value in fields:
            if name == "AppsAndFeaturesEntries":
                value = clean_entries(value, b"  ", version)
            if entry is x64[0] and name in ("ProductCode", "AppsAndFeaturesEntries"):
                extracted[name] = [line[2:] for line in value]
            else:
                rebuilt.extend(value)
        entry[:] = rebuilt
    present = {name for name in ("ProductCode", "AppsAndFeaturesEntries") if name in root}
    if present:
        if len(present) != 2 or extracted:
            fail("mixed root and x64 product metadata")
    elif set(extracted) == {"ProductCode", "AppsAndFeaturesEntries"}:
        root.update(extracted)
    else:
        fail("missing x64 product metadata")
    root["Installers"] = [raw[0]] + [line for entry in entries for line in entry]
    return b"".join(prefix + [line for name in ROOT_KEYS if name in root for line in root[name]])


def generate(stdout, output):
    pattern = rb"(?m)^# Created with [^\r\n]+\r\n.*?^ManifestVersion: [^\r\n]+\r\n"
    matches = list(re.finditer(pattern, stdout, re.DOTALL | re.MULTILINE))
    if not matches or not re.fullmatch(rb"Latest version of [^\r\n]+\n", stdout[:matches[0].start()]):
        fail("unrecognized komac output header")
    if any(stdout[a.end():b.start()] != b"\n" for a, b in zip(matches, matches[1:])) or stdout[matches[-1].end():] != b"\n":
        fail("unrecognized komac manifest separators")
    files = {}
    identity = None
    types = []
    for match in matches:
        data = match.group()
        if re.search(rb"(?<!\r)\n", data):
            fail("manifest does not use CRLF")
        identifier, version = field(data, b"PackageIdentifier"), field(data, b"PackageVersion")
        if not SAFE_NAME.fullmatch(identifier) or not SAFE_NAME.fullmatch(version):
            fail("unsafe package identifier or version")
        if identity is None:
            identity = (identifier, version)
        if identity != (identifier, version):
            fail("manifest package identifiers or versions differ")
        kind = field(data, b"ManifestType")
        types.append(kind)
        if kind == b"installer":
            name = identifier + b".installer.yaml"
            data = installer(data, version)
        elif kind in (b"defaultLocale", b"locale"):
            locale = field(data, b"PackageLocale")
            if not SAFE_NAME.fullmatch(locale):
                fail("unsafe locale")
            name = identifier + b".locale." + locale + b".yaml"
        elif kind == b"version":
            name = identifier + b".yaml"
        else:
            fail("unknown manifest type")
        if name in files:
            fail("duplicate manifest filename")
        files[name] = data
    if types.count(b"installer") != 1 or types.count(b"defaultLocale") != 1 or types.count(b"version") != 1:
        fail("expected installer, defaultLocale and version manifests")
    output.mkdir(parents=True, exist_ok=True)
    for name, data in files.items():
        (output / name.decode("ascii")).write_bytes(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("stdout", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        generate(args.stdout.read_bytes(), args.output)
    except (ValueError, OSError) as error:
        print(f"winget manifest: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
