// SPDX-License-Identifier: AGPL-3.0-only

//! Colours for `[page] accent`: the [`Colour`] value, WCAG contrast, the
//! page backgrounds an accent is read against, and how the dark scheme's
//! accent is derived from the light one.
//!
//! The backgrounds here are the stylesheet's `--bg` and `--panel` tokens
//! (`crates/tocsin/static/tocsin.css`); a test there keeps the two equal.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An sRGB colour written in hex: `"#0b5cad"`, or the short `"#06c"`.
/// Kept in its long lowercase form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Colour([u8; 3]);

/// The pattern every [`Colour`] matches, as published in the JSON Schema.
pub const COLOUR_PATTERN: &str = "^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$";

/// The contrast WCAG AA asks of body text against its background.
pub const AA_TEXT: f64 = 4.5;

/// The backgrounds text sits on in one colour scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scheme {
    /// `light` or `dark`, for messages.
    pub name: &'static str,
    /// `--bg`: the page itself.
    pub background: Colour,
    /// `--panel`: the surfaces groups, incidents and the status sit on.
    pub surface: Colour,
}

impl Scheme {
    /// The lower of `colour`'s contrasts against the background and the
    /// surface, and which of them it is against.
    #[must_use]
    pub fn weakest(&self, colour: Colour) -> (f64, &'static str, Colour) {
        let on_background = colour.contrast(self.background);
        let on_surface = colour.contrast(self.surface);
        if on_surface < on_background {
            (on_surface, "surface", self.surface)
        } else {
            (on_background, "background", self.background)
        }
    }

    /// Whether `colour` reads as text on both backgrounds.
    #[must_use]
    pub fn passes(&self, colour: Colour) -> bool {
        self.weakest(colour).0 >= AA_TEXT
    }
}

/// The light scheme's backgrounds.
pub const LIGHT: Scheme = Scheme {
    name: "light",
    background: Colour([0xf6, 0xf8, 0xfa]),
    surface: Colour([0xff, 0xff, 0xff]),
};

/// The dark scheme's backgrounds.
pub const DARK: Scheme = Scheme {
    name: "dark",
    background: Colour([0x0d, 0x11, 0x17]),
    surface: Colour([0x16, 0x1b, 0x22]),
};

/// How many steps toward white [`Colour::for_dark`] takes at most; the
/// last is white itself.
const STEPS: u16 = 32;

impl Colour {
    /// Parses `#rrggbb` or `#rgb`, either case.
    ///
    /// # Errors
    ///
    /// Returns a message when `text` is not a hex colour.
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid =
            || format!("invalid colour `{text}`: write it in hex, such as \"#0b5cad\" or \"#06c\"");
        let digits = text.strip_prefix('#').ok_or_else(invalid)?;
        if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        let channel = |hex: &str| u8::from_str_radix(hex, 16).map_err(|_| invalid());
        match digits.len() {
            6 => Ok(Colour([
                channel(&digits[0..2])?,
                channel(&digits[2..4])?,
                channel(&digits[4..6])?,
            ])),
            3 => {
                let short = |at: usize| channel(&digits[at..=at]).map(|value| value * 17);
                Ok(Colour([short(0)?, short(1)?, short(2)?]))
            }
            _ => Err(invalid()),
        }
    }

    /// WCAG relative luminance, from 0 (black) to 1 (white).
    #[must_use]
    pub fn luminance(self) -> f64 {
        let linear = |channel: u8| {
            let value = f64::from(channel) / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        let [red, green, blue] = self.0;
        0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
    }

    /// WCAG contrast ratio with `other`, from 1 to 21; the order does not
    /// matter.
    #[must_use]
    pub fn contrast(self, other: Colour) -> f64 {
        let (one, two) = (self.luminance(), other.luminance());
        (one.max(two) + 0.05) / (one.min(two) + 0.05)
    }

    /// The colour to show for this accent in the dark scheme: itself if it
    /// already reads on [`DARK`]'s backgrounds, otherwise mixed toward
    /// white in steps of 1/32 until it does. White reads on both, so this
    /// always ends; the hue stays, the colour gets lighter and softer.
    #[must_use]
    pub fn for_dark(self) -> Colour {
        (0..=STEPS)
            .map(|step| self.toward_white(step))
            .find(|candidate| DARK.passes(*candidate))
            .unwrap_or(Colour([0xff; 3]))
    }

    /// `step`/[`STEPS`] of the way to white.
    fn toward_white(self, step: u16) -> Colour {
        Colour(self.0.map(|channel| {
            let channel = u16::from(channel);
            let mixed = channel + (255 - channel) * step / STEPS;
            u8::try_from(mixed).unwrap_or(u8::MAX)
        }))
    }
}

impl fmt::Display for Colour {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [red, green, blue] = self.0;
        write!(formatter, "#{red:02x}{green:02x}{blue:02x}")
    }
}

impl Serialize for Colour {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Colour {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Colour::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Colour {
    fn schema_name() -> Cow<'static, str> {
        "Colour".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": COLOUR_PATTERN,
            "description": "A colour in hex: \"#0b5cad\" or \"#06c\"."
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_are_hex_in_either_length_and_case() {
        assert_eq!(Colour::parse("#0B5CAD").unwrap().to_string(), "#0b5cad");
        assert_eq!(Colour::parse("#06c").unwrap().to_string(), "#0066cc");
        for bad in [
            "",
            "#",
            "0b5cad",
            "#0b5cad80",
            "#0b5ca",
            "#ggg",
            "blue",
            "#+1f",
        ] {
            assert!(Colour::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn contrast_follows_wcag() {
        let black = Colour::parse("#000").unwrap();
        let white = Colour::parse("#fff").unwrap();
        assert!((black.contrast(white) - 21.0).abs() < 1e-9);
        assert!((white.contrast(black) - 21.0).abs() < 1e-9);
        assert!((white.contrast(white) - 1.0).abs() < 1e-9);
        // The figure WebAIM's checker gives for the default accent.
        let accent = Colour::parse("#0b5cad").unwrap();
        assert!((accent.contrast(white) - 6.67).abs() < 0.01);
    }

    #[test]
    fn the_dark_accent_is_the_lightest_step_needed_and_always_reads() {
        // Already readable in the dark: kept.
        let light = Colour::parse("#4493f8").unwrap();
        assert_eq!(light.for_dark(), light);
        // A dark accent is lightened only as far as it must be.
        let navy = Colour::parse("#0b5cad").unwrap();
        let derived = navy.for_dark();
        assert!(DARK.passes(derived), "{derived}");
        let step = (0..=STEPS)
            .find(|step| navy.toward_white(*step) == derived)
            .unwrap();
        assert!(step > 0 && !DARK.passes(navy.toward_white(step - 1)));
        // Even black gets there.
        assert!(DARK.passes(Colour::parse("#000").unwrap().for_dark()));
    }
}
