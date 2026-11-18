//! Terminal frames: the Agent Handler owns the VT state and sends the visible grid to the UI.

use serde::{Deserialize, Serialize};

pub const BOLD: u8 = 1;
pub const ITALIC: u8 = 2;
pub const UNDERLINE: u8 = 4;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    /// Lines scrolled back from the bottom (0 = live view).
    #[serde(default)]
    pub display_offset: u32,
    pub lines: Vec<Vec<Run>>,
}

/// A run of cells with the same style. Colors are 0xRRGGBB. The cursor is baked in.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Run {
    pub text: String,
    /// Number of terminal cells the run covers (wide chars count 2).
    #[serde(default)]
    pub cells: u16,
    pub fg: u32,
    pub bg: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub flags: u8,
}

fn is_zero(v: &u8) -> bool {
    *v == 0
}

/// A key press as the UI saw it. The Agent Handler turns it into bytes using the terminal mode.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct KeyInput {
    pub key: String,
    #[serde(default)]
    pub key_char: Option<String>,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
}
