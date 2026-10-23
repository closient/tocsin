// SPDX-License-Identifier: AGPL-3.0-only

//! `application/x-www-form-urlencoded` bodies, as a list of fields.

/// A submitted form's fields, in order. A checkbox group repeats its name.
pub struct Fields(Vec<(String, String)>);

impl Fields {
    /// Parses a urlencoded body. Bytes that are not UTF-8 are replaced.
    pub fn parse(body: &[u8]) -> Self {
        Fields(url::form_urlencoded::parse(body).into_owned().collect())
    }

    /// The first value of `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The first value of `name`, or empty.
    pub fn text(&self, name: &str) -> String {
        self.get(name).unwrap_or_default().to_owned()
    }

    /// Every value of `name`.
    pub fn all(&self, name: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .collect()
    }
}
