//! A searchable list for the composer's agent settings (provider, model, thinking, mode). Some
//! providers offer hundreds of models, which a plain menu can't navigate.

use std::rc::Rc;
use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{App, AppContext as _, Context, DismissEvent, Entity, SharedString, Task, Window};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};

use crate::composer::Choice;

/// Lists of up to this many choices show every choice without a search box.
const SEARCH_THRESHOLD: usize = 8;

pub(crate) type OnChoose = Rc<dyn Fn(String, &mut Window, &mut App)>;

pub(crate) struct ChoicePickerDelegate {
    title: SharedString,
    choices: Vec<Choice>,
    current: Option<String>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    on_choose: OnChoose,
}

impl ChoicePickerDelegate {
    fn all_matches(&self) -> Vec<StringMatch> {
        self.choices
            .iter()
            .enumerate()
            .map(|(index, choice)| StringMatch {
                candidate_id: index,
                string: choice.label.clone(),
                positions: Vec::new(),
                score: 0.,
            })
            .collect()
    }

    fn current_index(&self) -> usize {
        self.matches
            .iter()
            .position(|matched| {
                self.choices
                    .get(matched.candidate_id)
                    .is_some_and(|choice| Some(&choice.id) == self.current.as_ref())
            })
            .unwrap_or(0)
    }
}

/// A choice's description without a leading copy of its label, since providers often write
/// "Opus 5.5 · Latest release" for the model labelled "Opus 5.5".
fn shown_description(choice: &Choice) -> Option<String> {
    let description = choice.description.as_deref()?.trim();
    let rest = description
        .strip_prefix(choice.label.as_str())
        .map(|rest| rest.trim_start_matches([' ', '·', '-', '—', ':']))
        .unwrap_or(description);
    (!rest.is_empty()).then(|| rest.to_owned())
}

/// A picker over `choices` that opens on the current one and calls `on_choose` with the picked
/// choice's ID.
pub(crate) fn choice_picker(
    title: impl Into<SharedString>,
    choices: Vec<Choice>,
    current: Option<String>,
    on_choose: OnChoose,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Picker<ChoicePickerDelegate>> {
    let searchable = choices.len() > SEARCH_THRESHOLD;
    let mut delegate = ChoicePickerDelegate {
        title: title.into(),
        choices,
        current,
        matches: Vec::new(),
        selected_index: 0,
        on_choose,
    };
    delegate.matches = delegate.all_matches();
    delegate.selected_index = delegate.current_index();
    cx.new(|cx| {
        let picker = if searchable {
            Picker::list(delegate, window, cx)
        } else {
            Picker::nonsearchable_list(delegate, window, cx)
        };
        picker.initial_width(rems(26.)).max_height(rems(24.))
    })
}

impl PickerDelegate for ChoicePickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "Paseo agent setting"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        format!("Search {}…", self.title.to_lowercase()).into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(format!("No matching {}", self.title.to_lowercase()).into())
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        if query.trim().is_empty() {
            self.matches = self.all_matches();
            self.selected_index = self.current_index();
            return Task::ready(());
        }
        // Search the description too, so "latest" or "1M" finds a model.
        let candidates = self
            .choices
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                let text = match &choice.description {
                    Some(description) => format!("{} {description}", choice.label),
                    None => choice.label.clone(),
                };
                StringMatchCandidate::new(index, &text)
            })
            .collect::<Vec<_>>();
        let background = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = match_strings(
                &candidates,
                &query,
                false,
                true,
                500,
                &Default::default(),
                background,
            )
            .await;
            if let Err(error) = picker.update(cx, |picker, cx| {
                picker.delegate.matches = matches;
                picker.delegate.selected_index = 0;
                cx.notify();
            }) {
                log::debug!("Paseo setting picker closed: {error}");
            }
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let chosen = self
            .matches
            .get(self.selected_index)
            .and_then(|matched| self.choices.get(matched.candidate_id))
            .map(|choice| choice.id.clone());
        if let Some(chosen) = chosen {
            (self.on_choose)(chosen, window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let matched = self.matches.get(index)?;
        let choice = self.choices.get(matched.candidate_id)?;
        let label_positions = matched
            .positions
            .iter()
            .copied()
            .filter(|position| *position < choice.label.len())
            .collect::<Vec<_>>();
        let is_current = self.current.as_ref() == Some(&choice.id);
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .min_w_0()
                        .child(HighlightedLabel::new(choice.label.clone(), label_positions))
                        .when_some(shown_description(choice), |this, description| {
                            this.child(
                                Label::new(description)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        }),
                )
                .end_slot::<Icon>(is_current.then(|| {
                    Icon::new(IconName::Check)
                        .size(IconSize::Small)
                        .color(Color::Accent)
                })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::cell::RefCell;

    fn choice(id: &str, label: &str, description: Option<&str>) -> Choice {
        Choice {
            id: id.into(),
            label: label.into(),
            description: description.map(str::to_owned),
            is_default: false,
            color_tier: None,
        }
    }

    #[gpui::test]
    async fn choice_picker_filters_and_marks_current(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let chosen = Rc::new(RefCell::new(None));
        let choices = (0..12)
            .map(|index| choice(&format!("model-{index}"), &format!("Model {index}"), None))
            .chain([choice(
                "opus",
                "Opus 5.5",
                Some("Opus 5.5 · Latest release"),
            )])
            .collect::<Vec<_>>();
        let window = cx.add_empty_window();
        let picker = window.update(|window, cx| {
            let chosen = chosen.clone();
            choice_picker(
                "Model",
                choices,
                Some("model-3".into()),
                Rc::new(move |id, _, _| *chosen.borrow_mut() = Some(id)),
                window,
                cx,
            )
        });
        picker.read_with(window, |picker, _| {
            assert_eq!(
                picker.delegate.selected_index, 3,
                "opens on the current choice"
            );
        });
        picker.update_in(window, |picker, window, cx| {
            picker.update_matches("latest".into(), window, cx)
        });
        window.run_until_parked();
        picker.update_in(window, |picker, window, cx| {
            assert_eq!(picker.delegate.matches.len(), 1, "searches descriptions");
            picker.delegate.confirm(false, window, cx);
        });
        assert_eq!(chosen.borrow().as_deref(), Some("opus"));
    }

    #[test]
    fn descriptions_drop_a_repeated_label() {
        let shown = |label: &str, description: &str| {
            shown_description(&choice("id", label, Some(description)))
        };
        assert_eq!(
            shown("Opus 5.5", "Opus 5.5 · Latest release").as_deref(),
            Some("Latest release")
        );
        assert_eq!(
            shown("Opus 4.8 1M", "Opus 4.8 with 1M context window").as_deref(),
            Some("Opus 4.8 with 1M context window")
        );
        assert_eq!(shown("Auto", "Auto").as_deref(), None);
    }
}
