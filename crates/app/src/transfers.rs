//! The list of a session's file transfers, shared by the host panel and the viewer window.

use std::path::PathBuf;

use dari_proto::{TransferEnd, TransferId};
use dari_session::{Transfer, TransferDirection, TransferState};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{ActiveTheme, Sizable as _, StyledExt as _};
use gpui_kit::*;

use crate::text::text;

/// Finished transfers kept on screen; older ones drop off.
const MAX_FINISHED: usize = 5;

/// What the user asked of a transfer row.
#[derive(Debug, Clone)]
pub(crate) enum TransferAction {
    Accept(TransferId),
    /// Stops a transfer, or declines an offer.
    Cancel(TransferId),
    Reveal(PathBuf),
    ClearFinished,
}

/// A view that shows a [`TransferList`] and reacts to its buttons.
pub(crate) trait TransferActions: 'static + Sized {
    fn transfer_action(&mut self, action: TransferAction, cx: &mut Context<Self>);
}

/// Transfers of one session, in the order they started.
#[derive(Debug, Default)]
pub(crate) struct TransferList {
    transfers: Vec<Transfer>,
}

impl TransferList {
    pub(crate) fn update(&mut self, transfer: Transfer) {
        match self
            .transfers
            .iter_mut()
            .find(|known| known.id == transfer.id)
        {
            Some(known) => *known = transfer,
            None => self.transfers.push(transfer),
        }
        let finished = self
            .transfers
            .iter()
            .filter(|transfer| transfer.state.is_finished())
            .count();
        let mut excess = finished.saturating_sub(MAX_FINISHED);
        self.transfers.retain(|transfer| {
            if excess > 0 && transfer.state.is_finished() {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }

    pub(crate) fn clear(&mut self) {
        self.transfers.clear();
    }

    fn clear_finished(&mut self) {
        self.transfers
            .retain(|transfer| !transfer.state.is_finished());
    }

    /// Handles the actions that only touch the list itself; returns the others.
    pub(crate) fn apply(&mut self, action: TransferAction, cx: &App) -> Option<TransferAction> {
        match action {
            TransferAction::ClearFinished => {
                self.clear_finished();
                None
            }
            TransferAction::Reveal(path) => {
                cx.reveal_path(&path);
                None
            }
            other => Some(other),
        }
    }

    pub(crate) fn transfers(&self) -> &[Transfer] {
        &self.transfers
    }

    /// The list, or `None` when there is nothing to show.
    pub(crate) fn render<V: TransferActions>(&self, cx: &mut Context<V>) -> Option<Div> {
        if self.transfers.is_empty() {
            return None;
        }
        let any_finished = self
            .transfers
            .iter()
            .any(|transfer| transfer.state.is_finished());
        let mut list = div().v_flex().gap_2();
        for transfer in &self.transfers {
            list = list.child(row(transfer, cx));
        }
        if any_finished {
            list = list.child(
                div().h_flex().justify_end().child(
                    Button::new("transfers-clear")
                        .ghost()
                        .xsmall()
                        .label(text().clear_finished)
                        .on_click(cx.listener(|this: &mut V, _, _, cx| {
                            this.transfer_action(TransferAction::ClearFinished, cx);
                        })),
                ),
            );
        }
        Some(list)
    }
}

fn row<V: TransferActions>(transfer: &Transfer, cx: &mut Context<V>) -> Div {
    let id = transfer.id;
    let arrow = match transfer.direction {
        TransferDirection::Sending => "↑",
        TransferDirection::Receiving => "↓",
    };
    let mut status = div()
        .h_flex()
        .gap_2()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(format_size(transfer.size))
        .children(transfer.files.map(|files| text().folder_files(files)))
        .child(state_label(transfer));
    let mut buttons = div().h_flex().gap_1();
    let incoming_offer = transfer.direction == TransferDirection::Receiving
        && transfer.state == TransferState::Offered;
    if incoming_offer {
        status = status.child(text().incoming_file_prompt);
        buttons = buttons.child(
            Button::new(SharedString::from(format!("transfer-accept-{}", id.0)))
                .primary()
                .xsmall()
                .label(text().save_file)
                .on_click(cx.listener(move |this: &mut V, _, _, cx| {
                    this.transfer_action(TransferAction::Accept(id), cx);
                })),
        );
    }
    if !transfer.state.is_finished() {
        buttons = buttons.child(
            Button::new(SharedString::from(format!("transfer-cancel-{}", id.0)))
                .ghost()
                .xsmall()
                .label(if incoming_offer {
                    text().decline
                } else {
                    text().cancel
                })
                .on_click(cx.listener(move |this: &mut V, _, _, cx| {
                    this.transfer_action(TransferAction::Cancel(id), cx);
                })),
        );
    }
    if let Some(saved) = transfer.saved_to.clone() {
        buttons = buttons.child(
            Button::new(SharedString::from(format!("transfer-reveal-{}", id.0)))
                .ghost()
                .xsmall()
                .label(text().show_in_folder)
                .on_click(cx.listener(move |this: &mut V, _, _, cx| {
                    this.transfer_action(TransferAction::Reveal(saved.clone()), cx);
                })),
        );
    }

    let mut details = div()
        .v_flex()
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(
            div()
                .h_flex()
                .gap_2()
                .text_sm()
                .child(arrow)
                .child(div().truncate().child(transfer.name.clone())),
        )
        .child(status);
    if transfer.state == TransferState::InProgress {
        details = details.child(
            Progress::new(SharedString::from(format!("transfer-progress-{}", id.0)))
                .value(percent(transfer.transferred, transfer.size)),
        );
    }
    div()
        .h_flex()
        .gap_3()
        .items_center()
        .child(details)
        .child(buttons)
}

fn state_label(transfer: &Transfer) -> &'static str {
    let text = text();
    match (transfer.state, transfer.direction) {
        (TransferState::Offered, TransferDirection::Sending) => text.waiting_for_answer,
        (TransferState::Offered, TransferDirection::Receiving) => "",
        (TransferState::InProgress, TransferDirection::Sending) => text.sending,
        (TransferState::InProgress, TransferDirection::Receiving) => text.receiving,
        (TransferState::Completed, TransferDirection::Sending) => text.sent,
        (TransferState::Completed, TransferDirection::Receiving) => text.received,
        (TransferState::Ended(TransferEnd::Declined), _) => text.transfer_declined,
        (TransferState::Ended(TransferEnd::Cancelled), _) => text.transfer_cancelled,
        (TransferState::Ended(TransferEnd::Failed), _) => text.transfer_failed,
    }
}

#[expect(clippy::cast_precision_loss, reason = "display only")]
fn percent(done: u64, total: u64) -> f32 {
    if total == 0 {
        100.
    } else {
        done as f32 / total as f32 * 100.
    }
}

/// A byte count as people read it: `512 B`, `1.5 MB`.
#[expect(clippy::cast_precision_loss, reason = "display only")]
pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000. && unit < UNITS.len() - 1 {
        value /= 1000.;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's `test` attribute would shadow the standard one.
    use dari_proto::TransferId;
    use dari_session::{Transfer, TransferDirection, TransferState};

    use super::{MAX_FINISHED, TransferList, format_size};

    fn transfer(id: u64, state: TransferState) -> Transfer {
        Transfer {
            id: TransferId(id),
            direction: TransferDirection::Sending,
            name: format!("file-{id}"),
            size: 10,
            transferred: 0,
            state,
            saved_to: None,
            files: None,
        }
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1_500), "1.5 KB");
        assert_eq!(format_size(2_000_000_000), "2.0 GB");
    }

    #[test]
    fn updates_replace_and_old_finished_transfers_drop_off() {
        let mut list = TransferList::default();
        list.update(transfer(1, TransferState::Offered));
        list.update(transfer(1, TransferState::InProgress));
        assert_eq!(list.transfers().len(), 1);
        assert_eq!(list.transfers()[0].state, TransferState::InProgress);
        for id in 2..=8 {
            list.update(transfer(id, TransferState::Completed));
        }
        // The running transfer stays; only the newest finished ones remain.
        assert_eq!(list.transfers().len(), 1 + MAX_FINISHED);
        assert_eq!(list.transfers()[0].id, TransferId(1));
        assert_eq!(list.transfers()[1].id, TransferId(4));
        list.clear_finished();
        assert_eq!(list.transfers().len(), 1);
    }
}
