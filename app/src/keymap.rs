//! Key bindings.
//!
//! A binding resolves to a command id and nothing else: the shell never
//! implements an editor action itself (specification section 12). That also
//! makes the whole keymap testable without a window, which is what the tests at
//! the bottom of this file do.

use ls_core::CommandArgs;
use winit::event::Modifiers;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// What a key press resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// Run this command.
    Command(&'static str, CommandArgs),
    /// Insert the text the platform produced for this key.
    InsertText,
    /// No binding; ignore the key.
    None,
}

impl Binding {
    fn command(id: &'static str) -> Binding {
        Binding::Command(id, CommandArgs::None)
    }
}

/// Resolves a key press against the modifiers in effect.
pub fn resolve(key: &Key, modifiers: &Modifiers) -> Binding {
    let state = modifiers.state();
    let control = state.contains(ModifiersState::CONTROL);
    let shift = state.contains(ModifiersState::SHIFT);
    let alt = state.contains(ModifiersState::ALT);

    // Alt is not used by any Stage 1 binding; leaving it unhandled keeps
    // platform menu accelerators working.
    if alt {
        return Binding::None;
    }

    match key {
        Key::Named(named) => resolve_named(*named, control, shift),
        Key::Character(text) if control => resolve_control_character(text.as_str(), shift),
        // Plain typing: the platform already produced the right characters,
        // including dead keys and IME-free composition.
        Key::Character(_) => Binding::InsertText,
        _ => Binding::None,
    }
}

fn resolve_named(key: NamedKey, control: bool, shift: bool) -> Binding {
    let movement = |base: &'static str, select: &'static str| {
        Binding::command(if shift { select } else { base })
    };

    match key {
        NamedKey::ArrowLeft if control => movement("cursor.word_left", "cursor.word_left.select"),
        NamedKey::ArrowRight if control => {
            movement("cursor.word_right", "cursor.word_right.select")
        }
        NamedKey::ArrowLeft => movement("cursor.left", "cursor.left.select"),
        NamedKey::ArrowRight => movement("cursor.right", "cursor.right.select"),
        NamedKey::ArrowUp => movement("cursor.up", "cursor.up.select"),
        NamedKey::ArrowDown => movement("cursor.down", "cursor.down.select"),
        NamedKey::Home if control => {
            movement("cursor.document_start", "cursor.document_start.select")
        }
        NamedKey::End if control => movement("cursor.document_end", "cursor.document_end.select"),
        NamedKey::Home => movement("cursor.line_start", "cursor.line_start.select"),
        NamedKey::End => movement("cursor.line_end", "cursor.line_end.select"),
        NamedKey::PageUp if control => Binding::command("view.previous_tab"),
        NamedKey::PageDown if control => Binding::command("view.next_tab"),
        NamedKey::PageUp => movement("cursor.page_up", "cursor.page_up.select"),
        NamedKey::PageDown => movement("cursor.page_down", "cursor.page_down.select"),

        NamedKey::Backspace if control => Binding::command("edit.delete_word_backward"),
        NamedKey::Delete if control => Binding::command("edit.delete_word_forward"),
        NamedKey::Backspace => Binding::command("edit.delete_backward"),
        NamedKey::Delete => Binding::command("edit.delete_forward"),
        NamedKey::Enter => Binding::command("edit.insert_newline"),
        NamedKey::Tab if control && shift => Binding::command("view.previous_tab"),
        NamedKey::Tab if control => Binding::command("view.next_tab"),
        NamedKey::Tab => Binding::command("edit.insert_tab"),
        NamedKey::Space => Binding::InsertText,

        NamedKey::F12 => Binding::command("view.toggle_performance_overlay"),
        NamedKey::F9 => Binding::command("view.toggle_dev_panel"),
        NamedKey::F5 => Binding::command("diagnostics.duplicate_storm"),
        NamedKey::F6 => Binding::command("diagnostics.slow_load"),
        NamedKey::F7 => Binding::command("diagnostics.failing_load"),
        NamedKey::Escape => Binding::command("document.cancel_load"),
        _ => Binding::None,
    }
}

fn resolve_control_character(text: &str, shift: bool) -> Binding {
    match text.to_ascii_lowercase().as_str() {
        "n" => Binding::command("file.new"),
        "o" => Binding::command("file.open"),
        "s" if shift => Binding::command("file.save_as"),
        "s" => Binding::command("file.save"),
        "w" => Binding::command("file.close_tab"),
        "q" => Binding::command("app.quit"),
        "z" if shift => Binding::command("edit.redo"),
        "z" => Binding::command("edit.undo"),
        "y" => Binding::command("edit.redo"),
        "c" => Binding::command("edit.copy"),
        "x" => Binding::command("edit.cut"),
        "v" => Binding::command("edit.paste"),
        "a" => Binding::command("edit.select_all"),
        _ => Binding::None,
    }
}

/// Resolves a key press while a confirmation is on screen.
///
/// A confirmation owns the keyboard, so this is deliberately a separate, tiny
/// table rather than a mode inside [`resolve`].
pub fn resolve_prompt(key: &Key) -> Option<crate::app::PromptAnswer> {
    use crate::app::PromptAnswer;
    match key {
        Key::Named(NamedKey::Escape) => Some(PromptAnswer::Cancel),
        Key::Named(NamedKey::Enter) => Some(PromptAnswer::Save),
        Key::Character(text) => match text.to_ascii_lowercase().as_str() {
            "s" => Some(PromptAnswer::Save),
            "d" => Some(PromptAnswer::Discard),
            "c" => Some(PromptAnswer::Cancel),
            _ => None,
        },
        _ => None,
    }
}

/// Bindings shown in the status bar hint.
pub const HINTS: &str = "Ctrl+O open  Ctrl+S save  Ctrl+Z undo  F9 loading  F12 metrics";

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::SmolStr;

    fn modifiers(state: ModifiersState) -> Modifiers {
        Modifiers::from(state)
    }

    fn character(text: &str) -> Key {
        Key::Character(SmolStr::new(text))
    }

    fn command_of(binding: Binding) -> Option<&'static str> {
        match binding {
            Binding::Command(id, _) => Some(id),
            _ => None,
        }
    }

    #[test]
    fn plain_characters_are_inserted() {
        let none = modifiers(ModifiersState::empty());
        assert_eq!(resolve(&character("a"), &none), Binding::InsertText);
        assert_eq!(resolve(&Key::Named(NamedKey::Space), &none), Binding::InsertText);
    }

    #[test]
    fn control_shortcuts_map_to_commands() {
        let control = modifiers(ModifiersState::CONTROL);
        assert_eq!(command_of(resolve(&character("s"), &control)), Some("file.save"));
        assert_eq!(command_of(resolve(&character("z"), &control)), Some("edit.undo"));
        assert_eq!(command_of(resolve(&character("a"), &control)), Some("edit.select_all"));

        let control_shift = modifiers(ModifiersState::CONTROL | ModifiersState::SHIFT);
        assert_eq!(command_of(resolve(&character("s"), &control_shift)), Some("file.save_as"));
        assert_eq!(command_of(resolve(&character("z"), &control_shift)), Some("edit.redo"));
    }

    #[test]
    fn shift_turns_movement_into_selection() {
        let none = modifiers(ModifiersState::empty());
        let shift = modifiers(ModifiersState::SHIFT);
        assert_eq!(
            command_of(resolve(&Key::Named(NamedKey::ArrowRight), &none)),
            Some("cursor.right")
        );
        assert_eq!(
            command_of(resolve(&Key::Named(NamedKey::ArrowRight), &shift)),
            Some("cursor.right.select")
        );
    }

    #[test]
    fn control_arrows_move_by_word() {
        let control = modifiers(ModifiersState::CONTROL);
        assert_eq!(
            command_of(resolve(&Key::Named(NamedKey::ArrowLeft), &control)),
            Some("cursor.word_left")
        );
        let control_shift = modifiers(ModifiersState::CONTROL | ModifiersState::SHIFT);
        assert_eq!(
            command_of(resolve(&Key::Named(NamedKey::ArrowLeft), &control_shift)),
            Some("cursor.word_left.select")
        );
    }

    #[test]
    fn editing_keys_are_bound() {
        let none = modifiers(ModifiersState::empty());
        assert_eq!(
            command_of(resolve(&Key::Named(NamedKey::Backspace), &none)),
            Some("edit.delete_backward")
        );
        assert_eq!(
            command_of(resolve(&Key::Named(NamedKey::Enter), &none)),
            Some("edit.insert_newline")
        );
        assert_eq!(command_of(resolve(&Key::Named(NamedKey::Tab), &none)), Some("edit.insert_tab"));
        let control = modifiers(ModifiersState::CONTROL);
        assert_eq!(
            command_of(resolve(&Key::Named(NamedKey::Backspace), &control)),
            Some("edit.delete_word_backward")
        );
    }

    #[test]
    fn tab_switching_is_bound_to_control_tab() {
        let control = modifiers(ModifiersState::CONTROL);
        assert_eq!(
            command_of(resolve(&Key::Named(NamedKey::Tab), &control)),
            Some("view.next_tab")
        );
        let control_shift = modifiers(ModifiersState::CONTROL | ModifiersState::SHIFT);
        assert_eq!(
            command_of(resolve(&Key::Named(NamedKey::Tab), &control_shift)),
            Some("view.previous_tab")
        );
    }

    #[test]
    fn alt_combinations_are_left_to_the_platform() {
        let alt = modifiers(ModifiersState::ALT);
        assert_eq!(resolve(&character("f"), &alt), Binding::None);
    }

    #[test]
    fn every_bound_command_exists_in_the_registry() {
        let states = [
            ModifiersState::empty(),
            ModifiersState::SHIFT,
            ModifiersState::CONTROL,
            ModifiersState::CONTROL | ModifiersState::SHIFT,
        ];
        let named = [
            NamedKey::ArrowLeft,
            NamedKey::ArrowRight,
            NamedKey::ArrowUp,
            NamedKey::ArrowDown,
            NamedKey::Home,
            NamedKey::End,
            NamedKey::PageUp,
            NamedKey::PageDown,
            NamedKey::Backspace,
            NamedKey::Delete,
            NamedKey::Enter,
            NamedKey::Tab,
            NamedKey::F12,
            NamedKey::F9,
            NamedKey::F5,
            NamedKey::F6,
            NamedKey::F7,
            NamedKey::Escape,
        ];
        let characters = ["n", "o", "s", "w", "q", "z", "y", "c", "x", "v", "a"];

        for state in states {
            let modifiers = modifiers(state);
            for key in named {
                if let Binding::Command(id, _) = resolve(&Key::Named(key), &modifiers) {
                    assert!(
                        ls_core::commands::find(id).is_some(),
                        "{id} is bound but not registered"
                    );
                }
            }
            for text in characters {
                if let Binding::Command(id, _) = resolve(&character(text), &modifiers) {
                    assert!(
                        ls_core::commands::find(id).is_some(),
                        "{id} is bound but not registered"
                    );
                }
            }
        }
    }
}
