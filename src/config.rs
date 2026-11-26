//! Configuration: `[house]`, `[web]`, `[reminders]` plus the shared Matrix
//! sections.

use mxbot_common::config::{MatrixConfig, SecurityConfig, TimeOfDay};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct Config {
    pub matrix: MatrixConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    pub house: HouseConfig,
    #[serde(default)]
    pub web: Option<WebConfig>,
    #[serde(default)]
    pub reminders: ReminderConfig,
}

#[derive(Deserialize, Clone)]
pub struct HouseConfig {
    /// The house room: task cards live here, and tasks can be created here.
    pub room_id: String,
    /// Shown in the web interface's title.
    #[serde(default = "default_house_name")]
    pub name: String,
    /// Suggested locations (free text is allowed too).
    #[serde(default = "default_locations")]
    pub locations: Vec<String>,
    #[serde(default = "default_categories")]
    pub categories: Vec<Category>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Category {
    /// Short key used in `#tags` and stored on tasks, e.g. "repair".
    pub key: String,
    pub label: String,
    pub emoji: String,
}

impl HouseConfig {
    pub fn category(&self, key: &str) -> Option<&Category> {
        self.categories.iter().find(|c| c.key == key)
    }
}

fn default_house_name() -> String {
    "House".to_owned()
}

fn default_locations() -> Vec<String> {
    [
        "Kitchen",
        "Bathroom",
        "Living room",
        "Hallway",
        "Garden",
        "Basement",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

pub fn default_categories() -> Vec<Category> {
    [
        ("repair", "Repair", "🔧"),
        ("cleaning", "Cleaning", "🧽"),
        ("shopping", "Shopping", "🛒"),
        ("garden", "Garden", "🌱"),
        ("admin", "Admin", "📄"),
        ("other", "Other", "📌"),
    ]
    .into_iter()
    .map(|(key, label, emoji)| Category {
        key: key.to_owned(),
        label: label.to_owned(),
        emoji: emoji.to_owned(),
    })
    .collect()
}

#[derive(Deserialize, Clone)]
pub struct WebConfig {
    /// Address the web interface listens on, e.g. "0.0.0.0:8093".
    pub bind_addr: String,
    /// Public base URL, e.g. "https://house.example.org" (no trailing slash).
    pub public_url: String,
}

/// Defaults for the runtime reminder settings.
#[derive(Deserialize, Clone)]
pub struct ReminderConfig {
    #[serde(default = "yes")]
    pub digest_enabled: bool,
    #[serde(default = "default_digest_time")]
    pub digest_time: TimeOfDay,
    /// A claimed task untouched this many days counts as stuck.
    #[serde(default = "default_stale_days")]
    pub stale_days: u32,
    #[serde(default = "yes")]
    pub weekly_enabled: bool,
    #[serde(default = "default_weekly_day")]
    pub weekly_day: String,
    #[serde(default = "default_weekly_time")]
    pub weekly_time: TimeOfDay,
    /// Notify the whole room when an urgent task is created.
    #[serde(default = "yes")]
    pub urgent_pings_room: bool,
}

impl Default for ReminderConfig {
    fn default() -> Self {
        Self {
            digest_enabled: true,
            digest_time: default_digest_time(),
            stale_days: default_stale_days(),
            weekly_enabled: true,
            weekly_day: default_weekly_day(),
            weekly_time: default_weekly_time(),
            urgent_pings_room: true,
        }
    }
}

fn yes() -> bool {
    true
}
fn default_digest_time() -> TimeOfDay {
    TimeOfDay::new(9, 0)
}
fn default_stale_days() -> u32 {
    7
}
fn default_weekly_day() -> String {
    "sunday".to_owned()
}
fn default_weekly_time() -> TimeOfDay {
    TimeOfDay::new(18, 0)
}

/// Runtime settings (`!admin set <key> <value>`), persisted in
/// `store/settings.json`; `[reminders]` provides the defaults.
#[derive(Serialize, Deserialize, Clone)]
pub struct Settings {
    pub digest_enabled: bool,
    pub digest_time: TimeOfDay,
    pub stale_days: u32,
    pub weekly_enabled: bool,
    pub weekly_day: String,
    pub weekly_time: TimeOfDay,
    pub urgent_pings_room: bool,
}

impl Settings {
    pub fn from_config(c: &ReminderConfig) -> Self {
        Self {
            digest_enabled: c.digest_enabled,
            digest_time: c.digest_time,
            stale_days: c.stale_days,
            weekly_enabled: c.weekly_enabled,
            weekly_day: c.weekly_day.clone(),
            weekly_time: c.weekly_time,
            urgent_pings_room: c.urgent_pings_room,
        }
    }
}

pub const ADMIN_HELP: &str = "Settings: digest_enabled, digest_time (HH:MM), stale_days, \
weekly_enabled, weekly_day, weekly_time, urgent_pings_room";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_house_config_gets_useful_defaults() {
        let house: HouseConfig = toml::from_str("room_id = \"!r:x\"").unwrap();
        assert_eq!(house.name, "House");
        assert!(house.locations.contains(&"Kitchen".to_owned()));
        assert_eq!(house.category("repair").unwrap().emoji, "🔧");

        let reminders: ReminderConfig = toml::from_str("").unwrap();
        let settings = Settings::from_config(&reminders);
        assert!(settings.digest_enabled);
        assert_eq!(settings.stale_days, 7);
        assert_eq!(settings.weekly_day, "sunday");
    }

    #[test]
    fn the_example_config_parses() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        assert_eq!(config.house.categories.len(), 6);
        assert_eq!(config.web.unwrap().bind_addr, "0.0.0.0:8093");
        assert!(config.security.verification.allow_users_from_joined_rooms);
    }
}
