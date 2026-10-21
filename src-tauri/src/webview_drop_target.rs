//! Supplementary OLE drop targets for Locus webview windows.
//!
//! wry registers its WebView2 file drop target once, on the child HWNDs that
//! exist when the controller is created, and turns WebView2's own external
//! drop off. A webview created hidden and unfocused at runtime (workspace
//! windows, Workbench windows) has not created its content HWNDs at that
//! point, so the HWNDs under the cursor never get a target and every file drag
//! is refused (tauri-apps/tauri#14643, tauri-apps/wry#1639). After the page
//! loads, and whenever the window gains focus, this registers an equivalent
//! target on each webview HWND that still has none. Targets wry registered are
//! left in place.
//!
//! Like wry's targets, these are not revoked: the HWNDs, some of them owned by
//! the WebView2 browser process, are torn down together with the window.

use std::cell::Cell;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::OnceLock;

use serde::Serialize;
use tauri::{AppHandle, DragDropEvent, Emitter, Manager, PhysicalPosition, WindowEvent};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use windows::core::{implement, Ref, BOOL};
use windows::Win32::{
    Foundation::{DRAGDROP_E_ALREADYREGISTERED, DRAGDROP_E_INVALIDHWND, HWND, LPARAM, POINT, POINTL},
    Graphics::Gdi::ScreenToClient,
    System::{
        Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL},
        Ole::{
            IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, CF_HDROP,
            DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
        },
        SystemServices::MODIFIERKEYS_FLAGS,
    },
    UI::{
        Shell::{DragQueryFileW, HDROP},
        WindowsAndMessaging::{EnumChildWindows, GetAncestor, GetClassNameW, GA_PARENT},
    },
};

const WEBVIEW_CONTAINER_CLASS_NAME: &str = "WRY_WEBVIEW";
// The events Tauri emits for its own drop target (`onDragDropEvent`).
const TAURI_DRAG_ENTER_EVENT: &str = "tauri://drag-enter";
const TAURI_DRAG_OVER_EVENT: &str = "tauri://drag-over";
const TAURI_DRAG_DROP_EVENT: &str = "tauri://drag-drop";
const TAURI_DRAG_LEAVE_EVENT: &str = "tauri://drag-leave";

pub fn handle_window_event(window: &tauri::Window, event: &WindowEvent) {
    if matches!(event, WindowEvent::Focused(true)) {
        ensure(window);
    }
}

/// Registers the supplementary target on webview HWNDs that have no drop
/// target yet. Must run on the main (OLE) thread.
pub fn ensure(window: &tauri::Window) {
    if !crate::commands::is_locus_drop_target_label(window.label()) {
        return;
    }
    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    unsafe {
        let Some(container) = webview_container(HWND(hwnd.0 as _)) else {
            return;
        };
        let target: IDropTarget = SupplementaryDropTarget {
            app: window.app_handle().clone(),
            label: window.label().to_string(),
            container: container.0 as isize,
            accepts: Cell::new(false),
        }
        .into();
        for child in child_windows(container) {
            if let Err(error) = RegisterDragDrop(child, &target) {
                let code = error.code();
                if code != DRAGDROP_E_ALREADYREGISTERED && code != DRAGDROP_E_INVALIDHWND {
                    eprintln!(
                        "[Locus] failed to register webview drop target for {}: {error}",
                        window.label()
                    );
                }
            }
        }
    }
}

#[implement(IDropTarget)]
struct SupplementaryDropTarget {
    app: AppHandle,
    label: String,
    // Positions are client coordinates of the webview, as wry reports them.
    container: isize,
    accepts: Cell<bool>,
}

impl SupplementaryDropTarget {
    fn position(&self, point: &POINTL) -> PhysicalPosition<f64> {
        let mut point = POINT { x: point.x, y: point.y };
        let _ = unsafe { ScreenToClient(HWND(self.container as _), &mut point) };
        PhysicalPosition::new(point.x as f64, point.y as f64)
    }

    fn send(&self, event: DragDropEvent) {
        let _ = dispatcher().send(PendingDragDrop {
            app: self.app.clone(),
            label: self.label.clone(),
            event,
        });
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for SupplementaryDropTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<'_, IDataObject>,
        _key_state: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // A file list without entries is still a file drag: Unity scene
        // objects arrive this way and are resolved from the cached refs.
        let paths = unsafe { dropped_paths(&data) };
        self.accepts.set(paths.is_some());
        if let Some(paths) = paths {
            self.send(DragDropEvent::Enter {
                paths,
                position: self.position(point),
            });
        }
        unsafe { set_effect(effect, self.accepts.get()) };
        Ok(())
    }

    fn DragOver(
        &self,
        _key_state: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        if self.accepts.get() {
            self.send(DragDropEvent::Over {
                position: self.position(point),
            });
        }
        unsafe { set_effect(effect, self.accepts.get()) };
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        if self.accepts.replace(false) {
            self.send(DragDropEvent::Leave);
        }
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<'_, IDataObject>,
        _key_state: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let accepted = self.accepts.replace(false);
        if accepted {
            self.send(DragDropEvent::Drop {
                paths: unsafe { dropped_paths(&data) }.unwrap_or_default(),
                position: self.position(point),
            });
        }
        unsafe { set_effect(effect, accepted) };
        Ok(())
    }
}

struct PendingDragDrop {
    app: AppHandle,
    label: String,
    event: DragDropEvent,
}

#[derive(Clone, Serialize)]
struct TauriDragDropPayload<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    paths: Option<&'a [PathBuf]>,
    position: &'a PhysicalPosition<f64>,
}

/// OLE calls the target synchronously from the drag source; like wry, handle
/// the events afterwards, in order, outside that call.
fn dispatcher() -> &'static UnboundedSender<PendingDragDrop> {
    static DISPATCHER: OnceLock<UnboundedSender<PendingDragDrop>> = OnceLock::new();
    DISPATCHER.get_or_init(|| {
        let (sender, mut receiver) = unbounded_channel::<PendingDragDrop>();
        tauri::async_runtime::spawn(async move {
            while let Some(pending) = receiver.recv().await {
                dispatch(&pending);
            }
        });
        sender
    })
}

fn dispatch(pending: &PendingDragDrop) {
    let PendingDragDrop { app, label, event } = pending;
    let result = match event {
        DragDropEvent::Enter { paths, position } => app.emit_to(
            label.as_str(),
            TAURI_DRAG_ENTER_EVENT,
            TauriDragDropPayload { paths: Some(paths.as_slice()), position },
        ),
        DragDropEvent::Over { position } => app.emit_to(
            label.as_str(),
            TAURI_DRAG_OVER_EVENT,
            TauriDragDropPayload { paths: None, position },
        ),
        DragDropEvent::Drop { paths, position } => app.emit_to(
            label.as_str(),
            TAURI_DRAG_DROP_EVENT,
            TauriDragDropPayload { paths: Some(paths.as_slice()), position },
        ),
        DragDropEvent::Leave => app.emit_to(label.as_str(), TAURI_DRAG_LEAVE_EVENT, ()),
        _ => Ok(()),
    };
    if let Err(error) = result {
        eprintln!("[Locus] failed to emit webview drag event to {label}: {error}");
    }
    crate::commands::handle_locus_window_drag_drop(app, label, event);
}

unsafe fn set_effect(effect: *mut DROPEFFECT, accepted: bool) {
    if !effect.is_null() {
        unsafe { *effect = if accepted { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
    }
}

unsafe fn dropped_paths(data: &Ref<'_, IDataObject>) -> Option<Vec<PathBuf>> {
    let data = data.as_ref()?;
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut medium = unsafe { data.GetData(&format) }.ok()?;
    let hdrop = HDROP(unsafe { medium.u.hGlobal.0 });
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        let len = unsafe { DragQueryFileW(hdrop, index, None) } as usize;
        let mut buffer = vec![0u16; len + 1];
        let copied = unsafe { DragQueryFileW(hdrop, index, Some(&mut buffer)) } as usize;
        paths.push(PathBuf::from(OsString::from_wide(&buffer[..copied.min(len)])));
    }
    unsafe { ReleaseStgMedium(&mut medium) };
    Some(paths)
}

/// The single wry container directly under the window; its descendants are
/// the WebView2 HWNDs wry registers on.
unsafe fn webview_container(window: HWND) -> Option<HWND> {
    let mut containers = unsafe { child_windows(window) }.into_iter().filter(|&hwnd| unsafe {
        GetAncestor(hwnd, GA_PARENT) == window && class_name(hwnd) == WEBVIEW_CONTAINER_CLASS_NAME
    });
    let container = containers.next()?;
    containers.next().is_none().then_some(container)
}

unsafe fn class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 128];
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

unsafe fn child_windows(parent: HWND) -> Vec<HWND> {
    let mut children = Vec::<HWND>::new();
    let _ = unsafe {
        EnumChildWindows(
            Some(parent),
            Some(collect_child_window),
            LPARAM(&mut children as *mut Vec<HWND> as isize),
        )
    };
    children
}

unsafe extern "system" fn collect_child_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let children = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    children.push(hwnd);
    BOOL(1)
}
