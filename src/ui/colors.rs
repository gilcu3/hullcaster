use anyhow::{anyhow, Result};

use once_cell::sync::Lazy;
use ratatui::style::Color;
use regex::Regex;

use crate::config::AppColorsFromToml;

static RE_COLOR_HEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})").expect("Regex error"));

static RE_COLOR_RGB: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)rgb\(([0-9]+), ?([0-9]+), ?([0-9]+)\)").expect("Regex error"));

/// Holds information about the colors to use in the application. Tuple
/// values represent (foreground, background), respectively.
#[derive(Debug, Clone)]
pub struct AppColors {
    pub normal: (Color, Color),
    pub bold: (Color, Color),
    pub highlighted_active: (Color, Color),
    pub highlighted: (Color, Color),
    pub error: (Color, Color),
}

impl AppColors {
    /// Creates an AppColors struct with default color values.
    pub fn default() -> Self {
        Self {
            normal: (Color::Gray, Color::Black),
            bold: (Color::White, Color::Blue),
            highlighted_active: (Color::Black, Color::Yellow),
            highlighted: (Color::Black, Color::Gray),
            error: (Color::Red, Color::Black),
        }
    }

    /// Reading in values that were set in the config file, this changes
    /// the associated colors. Note that this only modifies colors that
    /// were set in the config, so this is most useful in conjunction
    /// with `default()` to set default colors and then change
    /// the ones that the user has set.
    pub fn add_from_config(&mut self, config: AppColorsFromToml) {
        if let Some(val) = config.normal_foreground {
            if let Ok(v) = Self::color_from_str(&val) {
                self.normal.0 = v;
            }
        }
        if let Some(val) = config.normal_background {
            if let Ok(v) = Self::color_from_str(&val) {
                self.normal.1 = v;
            }
        }
        if let Some(val) = config.bold_foreground {
            if let Ok(v) = Self::color_from_str(&val) {
                self.bold.0 = v;
            }
        }
        if let Some(val) = config.bold_background {
            if let Ok(v) = Self::color_from_str(&val) {
                self.bold.1 = v;
            }
        }
        if let Some(val) = config.highlighted_active_foreground {
            if let Ok(v) = Self::color_from_str(&val) {
                self.highlighted_active.0 = v;
            }
        }
        if let Some(val) = config.highlighted_active_background {
            if let Ok(v) = Self::color_from_str(&val) {
                self.highlighted_active.1 = v;
            }
        }
        if let Some(val) = config.highlighted_foreground {
            if let Ok(v) = Self::color_from_str(&val) {
                self.highlighted.0 = v;
            }
        }
        if let Some(val) = config.highlighted_background {
            if let Ok(v) = Self::color_from_str(&val) {
                self.highlighted.1 = v;
            }
        }
        if let Some(val) = config.error_foreground {
            if let Ok(v) = Self::color_from_str(&val) {
                self.error.0 = v;
            }
        }
        if let Some(val) = config.error_background {
            if let Ok(v) = Self::color_from_str(&val) {
                self.error.1 = v;
            }
        }
    }

    /// Parses a string that specifies a color either in hex format
    /// (e.g., "#ff0000"), in RGB format (e.g., "rgb(255, 0, 0)"), or
    /// as one of a set of allowed color names.
    pub fn color_from_str(text: &str) -> Result<Color> {
        if text.starts_with('#') {
            if let Some(cap) = RE_COLOR_HEX.captures(text) {
                return Ok(Color::Rgb(
                    u8::from_str_radix(&cap[1], 16)?,
                    u8::from_str_radix(&cap[2], 16)?,
                    u8::from_str_radix(&cap[3], 16)?,
                ));
            }
            Err(anyhow!("Invalid color hex code"))
        } else if text.starts_with("rgb") || text.starts_with("RGB") {
            #[allow(clippy::from_str_radix_10)]
            if let Some(cap) = RE_COLOR_RGB.captures(text) {
                return Ok(Color::Rgb(
                    u8::from_str_radix(&cap[1], 10)?,
                    u8::from_str_radix(&cap[2], 10)?,
                    u8::from_str_radix(&cap[3], 10)?,
                ));
            }
            return Err(anyhow!("Invalid color RGB code"));
        } else {
            let text_lower = text.to_lowercase();
            return match &text_lower[..] {
                "black" => Ok(Color::Black),
                "darkgrey" | "darkgray" => Ok(Color::DarkGray),
                "red" => Ok(Color::Red),
                "green" => Ok(Color::Green),
                "yellow" => Ok(Color::Yellow),
                "blue" => Ok(Color::Blue),
                "magenta" => Ok(Color::Magenta),
                "cyan" => Ok(Color::Cyan),
                "white" => Ok(Color::White),
                "grey" | "gray" => Ok(Color::Gray),
                "terminal" => Ok(Color::Reset),
                _ => Err(anyhow!("Invalid color code")),
            };
        }
    }
}

// TESTS -----------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_hex() {
        let color = String::from("#ff0000");
        let parsed = AppColors::color_from_str(&color);
        assert!(parsed.is_ok());
        assert_eq!(parsed.unwrap(), Color::Rgb(255, 0, 0));
    }

    #[test]
    fn color_invalid_hex() {
        let color = String::from("#gg0000");
        assert!(AppColors::color_from_str(&color).is_err());
    }

    #[test]
    fn color_invalid_hex2() {
        let color = String::from("#ff000");
        assert!(AppColors::color_from_str(&color).is_err());
    }

    #[test]
    fn color_rgb() {
        let color = String::from("rgb(255, 0, 0)");
        let parsed = AppColors::color_from_str(&color);
        assert!(parsed.is_ok());
        assert_eq!(parsed.unwrap(), Color::Rgb(255, 0, 0));
    }

    #[test]
    fn color_rgb_upper() {
        let color = String::from("RGB(255, 0, 0)");
        let parsed = AppColors::color_from_str(&color);
        assert!(parsed.is_ok());
        assert_eq!(parsed.unwrap(), Color::Rgb(255, 0, 0));
    }

    #[test]
    fn color_rgb_no_space() {
        let color = String::from("rgb(255,0,0)");
        let parsed = AppColors::color_from_str(&color);
        assert!(parsed.is_ok());
        assert_eq!(parsed.unwrap(), Color::Rgb(255, 0, 0));
    }
}
