use serde::{Deserialize, Serialize};

const ISSUE_BASE: &str = "https://github.com/jcardama/LeopardWM/issues/new?template=bug_report.yml";
const URL_LIMIT: usize = 2000;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct IssueReport {
    pub title: String,
    pub category: String,
    pub actual: String,
    pub expected: String,
    pub repro: String,
    pub affected_apps: String,
    pub regression: String,
}

fn encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(&mut encoded, "%{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    encoded
}

fn build_url(fields: &[(&str, String)]) -> String {
    let mut url = ISSUE_BASE.to_owned();
    for (key, value) in fields {
        if !value.is_empty() {
            url.push('&');
            url.push_str(key);
            url.push('=');
            url.push_str(&encode(value));
        }
    }
    url
}

pub fn issue_url(report: &IssueReport, version: &str, windows_version: &str) -> (String, bool) {
    let mut fields = [
        ("title", report.title.clone()),
        ("category", report.category.clone()),
        ("actual", report.actual.clone()),
        ("expected", report.expected.clone()),
        ("repro", report.repro.clone()),
        ("affected-apps", report.affected_apps.clone()),
        ("version", version.to_owned()),
        ("windows-version", windows_version.to_owned()),
        ("regression", report.regression.clone()),
    ];
    let mut url = build_url(&fields);
    let truncated = url.len() > URL_LIMIT;
    for index in [4, 3, 5, 2, 0, 1, 8, 7, 6] {
        if url.len() <= URL_LIMIT {
            break;
        }
        let original = std::mem::take(&mut fields[index].1);
        if original.is_empty() {
            continue;
        }
        let boundaries: Vec<usize> = original.char_indices().map(|(i, _)| i).collect();
        let mut low = 0;
        let mut high = boundaries.len();
        while low < high {
            let mid = (low + high).div_ceil(2);
            fields[index].1 = format!("{}…", &original[..boundaries[mid - 1]]);
            if build_url(&fields).len() <= URL_LIMIT {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        let shortened = if low == 0 {
            "…".to_owned()
        } else {
            format!("{}…", &original[..boundaries[low - 1]])
        };
        fields[index].1 = if encode(&shortened).len() < encode(&original).len() {
            shortened
        } else {
            original
        };
        url = build_url(&fields);
    }
    (url, truncated)
}

pub fn full_text(report: &IssueReport, version: &str, windows_version: &str) -> String {
    format!(
        "# {}\n\n## Category\n{}\n\n## What happened?\n{}\n\n## What did you expect to happen?\n{}\n\n## Steps to reproduce\n{}\n\n## Affected apps\n{}\n\n## LeopardWM version\n{}\n\n## Windows version\n{}\n\n## Did this work in an earlier version?\n{}\n",
        report.title, report.category, report.actual, report.expected, report.repro,
        report.affected_apps, version, windows_version, report.regression
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut result = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                result.push(u8::from_str_radix(&value[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                result.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(result).unwrap()
    }

    #[test]
    fn issue_url_preserves_short_values_and_matches_form_contract() {
        let report = IssueReport {
            title: "Title & = # + ? %\n中文😀".into(),
            category: "Focus".into(),
            actual: "Actual & = # + ? %\n中文😀".into(),
            expected: "Expected".into(),
            repro: "Steps".into(),
            affected_apps: "App".into(),
            regression: "Not sure".into(),
        };
        let (url, truncated) = issue_url(&report, "0.4.0", "Windows 11");
        assert!(!truncated);
        let pairs: Vec<_> = url
            .split_once('?')
            .unwrap()
            .1
            .split('&')
            .map(|p| p.split_once('=').unwrap())
            .collect();
        let decoded: std::collections::HashMap<_, _> =
            pairs.iter().map(|(k, v)| (*k, decode(v))).collect();
        assert_eq!(decoded["title"], report.title);
        assert_eq!(decoded["actual"], report.actual);
        assert_eq!(decoded["category"], report.category);
        assert_eq!(decoded["expected"], report.expected);
        assert_eq!(decoded["repro"], report.repro);
        assert_eq!(decoded["affected-apps"], report.affected_apps);
        assert_eq!(decoded["regression"], report.regression);
        assert_eq!(decoded["version"], "0.4.0");
        assert_eq!(decoded["windows-version"], "Windows 11");
        let form = include_str!("../../../../.github/ISSUE_TEMPLATE/bug_report.yml");
        let ids: HashSet<_> = form
            .lines()
            .filter_map(|line| line.trim().strip_prefix("id: "))
            .collect();
        for (key, _) in pairs {
            assert!(
                key == "template" || key == "title" || ids.contains(key),
                "unknown form field {key}"
            );
        }
        let lower = form.to_lowercase();
        for forbidden in ["linear", "leo-", "ticket", "tracker"] {
            assert!(!lower.contains(forbidden));
        }
        assert!(full_text(&report, "0.4.0", "Windows 11").contains(&report.actual));
    }

    #[test]
    fn long_unicode_reports_fit_without_broken_encodings() {
        for text in ["a", "中文", "😀", "&=#+?%\n"] {
            let report = IssueReport {
                title: text.repeat(3000),
                actual: text.repeat(3000),
                expected: text.repeat(3000),
                repro: text.repeat(3000),
                affected_apps: text.repeat(3000),
                ..IssueReport::default()
            };
            let (url, truncated) = issue_url(&report, "0.4.0", "Windows 11");
            assert!(truncated);
            assert!(url.len() <= URL_LIMIT);
            for pair in url.split_once('?').unwrap().1.split('&') {
                decode(pair.split_once('=').unwrap().1);
            }
            assert!(url.contains("%E2%80%A6"));
        }

        let report = IssueReport {
            title: "A report".into(),
            actual: "x".repeat(300),
            expected: "y".repeat(3000),
            repro: "z".repeat(3000),
            ..IssueReport::default()
        };
        let (url, truncated) = issue_url(&report, "0.4.0", "Windows 11");
        assert!(truncated);
        assert!(url.len() <= URL_LIMIT);
        let actual = url
            .split_once('?')
            .unwrap()
            .1
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == "actual")
            .map(|(_, value)| decode(value))
            .unwrap();
        assert_eq!(actual, report.actual);

        for report in [
            IssueReport {
                title: "Report".into(),
                actual: "x".repeat(3000),
                affected_apps: "Edge".into(),
                ..IssueReport::default()
            },
            IssueReport {
                title: "x".repeat(3000),
                actual: "x".into(),
                ..IssueReport::default()
            },
        ] {
            let (url, truncated) = issue_url(&report, "0.4.0", "Windows 11");
            assert!(truncated);
            assert!(url.len() <= URL_LIMIT);
            let decoded: std::collections::HashMap<_, _> = url
                .split_once('?')
                .unwrap()
                .1
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .map(|(key, value)| (key, decode(value)))
                .collect();
            if !report.affected_apps.is_empty() {
                assert_eq!(decoded["affected-apps"], report.affected_apps);
            } else {
                assert_eq!(decoded["actual"], report.actual);
                assert!(decoded["title"].ends_with('…'));
            }
        }
    }
}
