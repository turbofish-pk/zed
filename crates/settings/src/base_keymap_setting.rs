use std::fmt::{Display, Formatter};

use crate::{self as settings, settings_content::BaseKeymapContent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings};

/// Base key bindings scheme. Base keymaps can be overridden with user keymaps.
///
/// Default: Zed
#[derive(
    Copy, Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default, RegisterSetting,
)]
pub enum BaseKeymap {
    #[default]
    Zed,
    VSCode,
    JetBrains,
    SublimeText,
    None,
}

impl From<BaseKeymapContent> for BaseKeymap {
    fn from(value: BaseKeymapContent) -> Self {
        match value {
            BaseKeymapContent::Zed => Self::Zed,
            BaseKeymapContent::VSCode => Self::VSCode,
            BaseKeymapContent::JetBrains => Self::JetBrains,
            BaseKeymapContent::SublimeText => Self::SublimeText,
            BaseKeymapContent::None => Self::None,
        }
    }
}
impl Into<BaseKeymapContent> for BaseKeymap {
    fn into(self) -> BaseKeymapContent {
        match self {
            BaseKeymap::Zed => BaseKeymapContent::Zed,
            BaseKeymap::VSCode => BaseKeymapContent::VSCode,
            BaseKeymap::JetBrains => BaseKeymapContent::JetBrains,
            BaseKeymap::SublimeText => BaseKeymapContent::SublimeText,
            BaseKeymap::None => BaseKeymapContent::None,
        }
    }
}

impl Display for BaseKeymap {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            BaseKeymap::Zed => write!(f, "Zed"),
            BaseKeymap::VSCode => write!(f, "VS Code"),
            BaseKeymap::JetBrains => write!(f, "JetBrains"),
            BaseKeymap::SublimeText => write!(f, "Sublime Text"),
            BaseKeymap::None => write!(f, "None"),
        }
    }
}

impl BaseKeymap {

    pub const OPTIONS: [(&'static str, Self); 4] = [
        ("Zed (Default)", Self::Zed),
        ("VS Code", Self::VSCode),
        ("JetBrains", Self::JetBrains),
        ("Sublime Text", Self::SublimeText)
    ];

    pub fn asset_path(&self) -> Option<&'static str> {

        
        match self {
            BaseKeymap::JetBrains => Some("keymaps/linux/jetbrains.json"),
            BaseKeymap::SublimeText => Some("keymaps/linux/sublime_text.json"),
            BaseKeymap::VSCode => Some("keymaps/linux/vscode.json"),
            BaseKeymap::Zed => None,
            BaseKeymap::None => None,
        }
    }

    pub fn names() -> impl Iterator<Item = &'static str> {
        Self::OPTIONS.iter().map(|(name, _)| *name)
    }

    pub fn from_names(option: &str) -> BaseKeymap {
        Self::OPTIONS
            .iter()
            .copied()
            .find_map(|(name, value)| (name == option).then_some(value))
            .unwrap_or_default()
    }
}

impl Settings for BaseKeymap {
    fn from_settings(s: &crate::settings_content::SettingsContent) -> Self {
        s.base_keymap.unwrap().into()
    }
}
