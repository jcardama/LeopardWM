# Distribution Setup

Manual one-time steps to enable winget auto-publishing and code signing. Each section is self-contained — do them in any order.

## winget — `WINGET_TOKEN` secret

The `publish-winget` job in `.github/workflows/release.yml` verifies pinned komac 2.17.1, generates manifests from both released MSIs, and adjusts the installer metadata for winget-pkgs. It creates a branch and one manifest commit on the existing `jcardama/winget-pkgs` fork, then opens a PR to `microsoft/winget-pkgs`. The fork must already exist.

Use a classic personal access token with `public_repo` and `workflow`. The new fork ref can include upstream workflow changes absent from the fork, which requires `workflow`. Fine-grained tokens cannot open the upstream PR.

1. Go to https://github.com/settings/tokens/new.
2. Set the note to `LeopardWM winget releaser` and expiration to `1 year` (renew when it expires).
3. Select `public_repo` and `workflow`, generate the token, and copy it.
4. Add it as the repository secret:
   ```
   gh secret set WINGET_TOKEN --repo jcardama/LeopardWM
   # (paste the token when prompted)
   ```

**Bootstrap:** komac update requires `jcardama.LeopardWM` to exist upstream already. The first manual submission in v0.1.11 seeded it.

**Verify:** after pushing a `v*` tag, check the `publish-winget` job at https://github.com/jcardama/LeopardWM/actions. It prints the PR URL on success. Check that PR and its validation results; a successful GitHub Release alone does not prove the Winget job succeeded.

**Manual fallback:** if the job fails, inspect the job log and any existing open PR or fork branch before retrying. Run the pinned komac `update jcardama.LeopardWM --version <version> --urls <x64-msi-url> <arm64-msi-url> --dry-run` locally with `GITHUB_TOKEN` set, preserving stdout bytes in a file. Run `python tools/winget_manifest.py <stdout-file> <output-directory>`, then `winget validate --manifest <output-directory>`. Submit the validated files by hand to `microsoft/winget-pkgs`. Do not use `komac submit`; it rewrites the installer layout.

---

## Code signing — IDs.cloud verification + Azure Trusted Signing

This eliminates the SmartScreen "Windows protected your PC" prompt on first install. komorebi doesn't have this; GlazeWM does. Without it, every first-time installer hits the prompt and many users abandon.

Two parts: **publisher identity verification** (your part, ~$33, ~1 week async) and **Azure Trusted Signing setup** (we wire into CI once verification clears).

### Step 1 — Identity verification (you do this)

The lowest-cost path for an individual OSS dev:

1. Go to https://www.ids.cloud/identity-verification (or alternative verifier accepted by Azure Trusted Signing — check current list at https://learn.microsoft.com/azure/trusted-signing/concept-trusted-signing-trust-models)
2. Choose **Individual Verification** (~$33). DUNS at $100+ is only needed if signing under a company name.
3. Submit the requested documents (government ID, proof of address). Turnaround is typically 3–7 business days.
4. On approval you'll receive a `MS Identity Validation` confirmation. Save the validation ID.

### Step 2 — Azure Trusted Signing account (you do this; ~10 min after Step 1 clears)

1. Sign in to https://portal.azure.com
2. Search for **Trusted Signing Accounts** → **Create**
3. Pick a region (East US is fine), resource group, and a unique account name like `leopardwm-signing`
4. Tier: **Basic** (~$10/mo)
5. After creation, go to the account → **Identity Validation** → submit the IDs.cloud validation ID from Step 1
6. Once Microsoft accepts (usually within a day), create a **Certificate Profile**:
   - Name: `leopardwm`
   - Type: **Public Trust** → **Individual**
   - Subject: matches your verified identity
7. Note these for the next step:
   - `AZURE_ENDPOINT` (the account's regional endpoint, e.g. `https://eus.codesigning.azure.net`)
   - `AZURE_CODE_SIGNING_NAME` (the account name)
   - `AZURE_CERT_PROFILE_NAME` (the certificate profile name)

### Step 3 — Wire into CI (I do this once you finish Step 2)

When you have the values from Step 2, share them and I'll:

1. Set up an Azure App Registration with federated credentials for GitHub Actions OIDC
2. Add `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_SUBSCRIPTION_ID`, plus the three values above as repo secrets
3. Add an `AzureSignTool` step to `.github/workflows/release.yml` that signs both the MSI and the bundled exes before they're attached to the release
4. Tag a release-candidate (`v0.2.0-rc1`) to verify end-to-end: install the signed MSI on a clean VM and confirm no SmartScreen prompt

---

## Status checklist

- [ ] WINGET_TOKEN secret created (Section 1)
- [ ] IDs.cloud individual verification submitted (Section 2, Step 1)
- [ ] IDs.cloud verification approved (~3–7 days async)
- [ ] Azure Trusted Signing account + cert profile created (Section 2, Step 2)
- [ ] Repo secrets + AzureSignTool step wired (Section 2, Step 3)
- [ ] v0.2.0-rc1 tagged, signing verified on clean VM
