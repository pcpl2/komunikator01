//! Draws the model's `View` into the window's controls, touching only what changed since the
//! last draw: a speaking change rewrites item texts, never the whole tree.

use std::collections::VecDeque;

use client_core::app::{ChatView, MemberView, RoomView, View};
use protocol::{MemberId, RoomId};
use windows::core::PWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::Controls::{
    HTREEITEM, LVITEMW, LVIF_TEXT, LVM_DELETEALLITEMS, LVM_DELETEITEM, LVM_ENSUREVISIBLE,
    LVM_INSERTITEMW, LVM_SETCOLUMNWIDTH, SB_SETTEXTW, TREE_VIEW_ITEM_STATE_FLAGS, TVE_EXPAND,
    TVINSERTSTRUCTW, TVINSERTSTRUCTW_0, TVIF_PARAM, TVIF_STATE, TVIF_TEXT, TVITEMW, TVIS_BOLD,
    TVI_LAST, TVI_ROOT, TVM_DELETEITEM, TVM_EXPAND, TVM_INSERTITEMW, TVM_SETITEMW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, SendMessageW, SetWindowTextW, WM_SETREDRAW};

use super::text::{
    chat_diff, chat_rows, deafen_label, member_text, mute_label, room_text, same_tree_shape,
    status_text, TreeItem,
};
use super::wide::wide;

/// The child controls the view is drawn into.
pub struct Controls {
    pub mute: HWND,
    pub deafen: HWND,
    pub leave: HWND,
    pub new_room: HWND,
    pub settings: HWND,
    pub top: HWND,
    pub tree: HWND,
    pub chat: HWND,
    pub input: HWND,
    pub send: HWND,
    pub status: HWND,
}

/// What is on screen now.
#[derive(Default)]
pub struct Render {
    version: Option<u64>,
    muted: Option<bool>,
    deafened: Option<bool>,
    in_room: Option<bool>,
    tree_built: bool,
    rooms: Vec<RoomView>,
    my_room: Option<RoomId>,
    /// Per room: its item and its members' items, in view order.
    items: Vec<(HTREEITEM, Vec<HTREEITEM>)>,
    chat: Vec<ChatView>,
    /// List rows per shown message, in order.
    chat_rows: VecDeque<usize>,
    status: String,
    view_notice: Option<String>,
    notice: Option<String>,
}

impl Render {
    pub fn draw(&mut self, c: &Controls, v: &View, version: u64, local_notice: Option<String>) {
        if let Some(n) = local_notice {
            self.set_notice(c, Some(n));
        }
        if self.version == Some(version) {
            return;
        }
        self.version = Some(version);
        if self.muted != Some(v.muted) {
            self.muted = Some(v.muted);
            set_text(c.mute, mute_label(v.muted));
        }
        if self.deafened != Some(v.deafened) {
            self.deafened = Some(v.deafened);
            set_text(c.deafen, deafen_label(v.deafened));
        }
        let in_room = v.my_room.is_some();
        if self.in_room != Some(in_room) {
            self.in_room = Some(in_room);
            let _ = unsafe { EnableWindow(c.leave, in_room) };
        }
        self.draw_tree(c.tree, &v.rooms, v.my_room);
        self.draw_chat(c.chat, &v.chat);
        let status = status_text(&v.status);
        if status != self.status {
            set_part(c.status, 0, &status);
            self.status = status;
        }
        // A model notice replaces a local one only when it changes.
        if v.notice != self.view_notice {
            self.view_notice = v.notice.clone();
            self.set_notice(c, v.notice.clone());
        }
    }

    /// The links of the message shown on list row `row`.
    pub fn links_at(&self, row: usize) -> &[String] {
        let mut end = 0;
        for (i, n) in self.chat_rows.iter().enumerate() {
            end += n;
            if row < end {
                return &self.chat[i].links;
            }
        }
        &[]
    }

    /// A member as last drawn, with whether they are in my room.
    pub fn member(&self, id: MemberId) -> Option<(&MemberView, bool)> {
        self.rooms.iter().find_map(|r| {
            r.members.iter().find(|m| m.id == id).map(|m| (m, Some(r.id) == self.my_room))
        })
    }

    /// The notice on the status bar now.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    fn set_notice(&mut self, c: &Controls, n: Option<String>) {
        if n != self.notice {
            set_part(c.status, 1, n.as_deref().unwrap_or(""));
            self.notice = n;
        }
    }

    fn draw_tree(&mut self, tree: HWND, rooms: &[RoomView], my_room: Option<RoomId>) {
        if self.tree_built && rooms == self.rooms && my_room == self.my_room {
            return;
        }
        if self.tree_built && my_room == self.my_room && same_tree_shape(&self.rooms, rooms) {
            for ((room, old), (_, members)) in rooms.iter().zip(&self.rooms).zip(&self.items) {
                let mine = Some(room.id) == my_room;
                for ((m, was), item) in room.members.iter().zip(&old.members).zip(members) {
                    if m != was {
                        set_item_text(tree, *item, &member_text(m, mine));
                    }
                }
            }
        } else {
            redraw(tree, false);
            unsafe { SendMessageW(tree, TVM_DELETEITEM, None, Some(LPARAM(TVI_ROOT.0))) };
            self.items = rooms
                .iter()
                .map(|r| {
                    let mine = Some(r.id) == my_room;
                    let room = insert_item(tree, TVI_ROOT, &room_text(r), TreeItem::Room(r.id), mine);
                    let members = r
                        .members
                        .iter()
                        .map(|m| insert_item(tree, room, &member_text(m, mine), TreeItem::Member(m.id), false))
                        .collect();
                    unsafe { SendMessageW(tree, TVM_EXPAND, Some(WPARAM(TVE_EXPAND.0 as usize)), Some(LPARAM(room.0))) };
                    (room, members)
                })
                .collect();
            redraw(tree, true);
        }
        self.rooms = rooms.to_vec();
        self.my_room = my_room;
        self.tree_built = true;
    }

    fn draw_chat(&mut self, list: HWND, chat: &[ChatView]) {
        if chat == self.chat.as_slice() {
            return;
        }
        let diff = chat_diff(&self.chat, chat);
        redraw(list, false);
        let gone: usize = self.chat_rows.drain(..diff.dropped).sum();
        if self.chat_rows.is_empty() {
            unsafe { SendMessageW(list, LVM_DELETEALLITEMS, None, None) };
        } else {
            for _ in 0..gone {
                unsafe { SendMessageW(list, LVM_DELETEITEM, Some(WPARAM(0)), None) };
            }
        }
        let mut row: usize = self.chat_rows.iter().sum();
        for line in &chat[diff.kept..] {
            let rows = chat_rows(line);
            for text in &rows {
                insert_row(list, row, text);
                row += 1;
            }
            self.chat_rows.push_back(rows.len());
        }
        if row > 0 {
            unsafe { SendMessageW(list, LVM_ENSUREVISIBLE, Some(WPARAM(row - 1)), Some(LPARAM(0))) };
        }
        redraw(list, true);
        fit_chat_column(list);
        self.chat = chat.to_vec();
    }
}

/// One full-width column; re-run when a scroll bar appears or the list is resized.
pub fn fit_chat_column(list: HWND) {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(list, &mut rc) };
    unsafe { SendMessageW(list, LVM_SETCOLUMNWIDTH, Some(WPARAM(0)), Some(LPARAM(rc.right as isize))) };
}

pub fn set_text(hwnd: HWND, text: &str) {
    let w = wide(text);
    let _ = unsafe { SetWindowTextW(hwnd, windows::core::PCWSTR(w.as_ptr())) };
}

fn set_part(status: HWND, part: usize, text: &str) {
    let w = wide(text);
    unsafe { SendMessageW(status, SB_SETTEXTW, Some(WPARAM(part)), Some(LPARAM(w.as_ptr() as isize))) };
}

fn redraw(hwnd: HWND, on: bool) {
    unsafe {
        SendMessageW(hwnd, WM_SETREDRAW, Some(WPARAM(on as usize)), None);
        if on {
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
    }
}

fn insert_item(tree: HWND, parent: HTREEITEM, text: &str, item: TreeItem, bold: bool) -> HTREEITEM {
    let mut w = wide(text);
    let ins = TVINSERTSTRUCTW {
        hParent: parent,
        hInsertAfter: TVI_LAST,
        Anonymous: TVINSERTSTRUCTW_0 {
            item: TVITEMW {
                mask: TVIF_TEXT | TVIF_PARAM | TVIF_STATE,
                state: if bold { TVIS_BOLD } else { TREE_VIEW_ITEM_STATE_FLAGS(0) },
                stateMask: TVIS_BOLD,
                pszText: PWSTR(w.as_mut_ptr()),
                lParam: LPARAM(item.to_param()),
                ..Default::default()
            },
        },
    };
    let r = unsafe { SendMessageW(tree, TVM_INSERTITEMW, None, Some(LPARAM(&ins as *const _ as isize))) };
    HTREEITEM(r.0)
}

fn set_item_text(tree: HWND, item: HTREEITEM, text: &str) {
    let mut w = wide(text);
    let it = TVITEMW { mask: TVIF_TEXT, hItem: item, pszText: PWSTR(w.as_mut_ptr()), ..Default::default() };
    unsafe { SendMessageW(tree, TVM_SETITEMW, None, Some(LPARAM(&it as *const _ as isize))) };
}

fn insert_row(list: HWND, row: usize, text: &str) {
    let mut w = wide(text);
    let it = LVITEMW { mask: LVIF_TEXT, iItem: row as i32, pszText: PWSTR(w.as_mut_ptr()), ..Default::default() };
    unsafe { SendMessageW(list, LVM_INSERTITEMW, None, Some(LPARAM(&it as *const _ as isize))) };
}
