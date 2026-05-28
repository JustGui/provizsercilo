use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageProfile {
    /// ISO 639-1 language code or "*" for wildcard.
    pub language: String,
    /// ISO 3166-1 alpha-2 country code or "*" for wildcard.
    pub country: String,
    /// Provider slugs in preferred order (first = highest boost).
    #[serde(default)]
    pub priority: Vec<String>,
    /// Provider slugs to exclude entirely for this locale.
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl LanguageProfile {
    fn specificity(&self) -> u8 {
        match (self.language.as_str(), self.country.as_str()) {
            (l, c) if l != "*" && c != "*" => 3,
            (l, _) if l != "*" => 2,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProfileMatcher {
    profiles: Vec<LanguageProfile>,
}

impl ProfileMatcher {
    pub fn new(profiles: Vec<LanguageProfile>) -> Self {
        let mut p = profiles;
        // Stable sort: most specific first, then original order within same specificity.
        p.sort_by(|a, b| b.specificity().cmp(&a.specificity()));
        Self { profiles: p }
    }

    pub fn load_toml(content: &str) -> Result<Self, toml::de::Error> {
        #[derive(Deserialize)]
        struct Config {
            profiles: Vec<LanguageProfile>,
        }
        let cfg: Config = toml::from_str(content)?;
        Ok(Self::new(cfg.profiles))
    }

    /// Find the best matching profile for the given language/country pair.
    /// Match order: (lang, country) > (lang, *) > (*, *)
    pub fn find(&self, language: Option<&str>, country: Option<&str>) -> Option<&LanguageProfile> {
        let lang = language.unwrap_or("*");
        let cty = country.unwrap_or("*");

        // Try exact match, then language-only, then catch-all.
        for target_spec in [3u8, 2, 1] {
            for p in &self.profiles {
                if p.specificity() != target_spec {
                    continue;
                }
                let lang_match = p.language == "*" || p.language == lang;
                let cty_match = p.country == "*" || p.country == cty;
                if lang_match && cty_match {
                    return Some(p);
                }
            }
        }
        None
    }

    /// Coverage boost for a provider slug based on its position in the priority list.
    /// Returns a value in [0, 1]. First position = 1.0, last = 1/n.
    pub fn coverage_boost(profile: &LanguageProfile, provider_slug: &str) -> f64 {
        let n = profile.priority.len();
        if n == 0 {
            return 0.0;
        }
        profile
            .priority
            .iter()
            .position(|s| s == provider_slug)
            .map(|pos| (n - pos) as f64 / n as f64)
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_specificity_order() {
        let matcher = ProfileMatcher::new(vec![
            LanguageProfile {
                language: "*".into(),
                country: "*".into(),
                priority: vec!["a".into()],
                exclude: vec![],
            },
            LanguageProfile {
                language: "fr".into(),
                country: "*".into(),
                priority: vec!["b".into()],
                exclude: vec![],
            },
            LanguageProfile {
                language: "fr".into(),
                country: "fr".into(),
                priority: vec!["c".into()],
                exclude: vec![],
            },
        ]);

        let p = matcher.find(Some("fr"), Some("fr")).unwrap();
        assert_eq!(p.priority[0], "c");

        let p = matcher.find(Some("fr"), Some("be")).unwrap();
        assert_eq!(p.priority[0], "b");

        let p = matcher.find(Some("de"), None).unwrap();
        assert_eq!(p.priority[0], "a");
    }
}
