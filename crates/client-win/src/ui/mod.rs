//! The native window: main window, tray, and their Win32 building blocks (hotkeys, sounds,
//! tray icons, clipboard, autostart). Pure helpers (parsing, tone synthesis, pixels, view
//! texts) are unit-testable.

pub mod app;
pub mod autostart;
pub mod clipboard;
pub mod dialogs;
pub mod hotkey;
pub mod icons;
pub mod instance;
mod layout;
mod render;
pub mod sound;
pub mod text;
pub mod tray;
pub mod wide;
pub mod window;
