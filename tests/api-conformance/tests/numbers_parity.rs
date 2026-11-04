//! The SDK's syscall numbers against the kernel's.
//!
//! The SDK mirrors the kernel's numbers in `sdk/vela-sys/src/numbers.rs`
//! because the two are separate compilation units. That mirror is only a
//! contract if something checks it: this file compares every syscall number
//! and every error code the SDK publishes against the kernel's own table.

use vela_hal_common::PAGE_SIZE;
use vela_kernel_syscall::numbers as kernel;
use vela_sys::numbers as sdk;

/// Every syscall the SDK names, paired with the kernel's constant of the same
/// meaning. The names differ by prefix, so the pair is explicit rather than
/// derived from a spelling rule.
#[test]
fn the_sdk_syscall_numbers_match_the_kernel_numbers() {
    let pairs: [(u64, u64); 35] = [
        (sdk::CREATE_MSG_PORT, kernel::CREATE_MSG_PORT),
        (sdk::DELETE_MSG_PORT, kernel::DELETE_MSG_PORT),
        (sdk::PUT_MSG, kernel::PUT_MSG),
        (sdk::GET_MSG, kernel::GET_MSG),
        (sdk::REPLY_MSG, kernel::REPLY_MSG),
        (sdk::WAIT_PORT, kernel::WAIT_PORT),
        (sdk::WAIT, kernel::WAIT),
        (sdk::SET_SIGNAL, kernel::SET_SIGNAL),
        (sdk::SET_EXCEPT, kernel::SET_EXCEPT),
        (sdk::SIGNAL, kernel::SIGNAL),
        (sdk::OPEN_LIBRARY, kernel::OPEN_LIBRARY),
        (sdk::CLOSE_LIBRARY, kernel::CLOSE_LIBRARY),
        (sdk::FORBID, kernel::FORBID),
        (sdk::PERMIT, kernel::PERMIT),
        (sdk::YIELD, kernel::YIELD),
        (sdk::GET_TASK_ID, kernel::GET_TASK_ID),
        (sdk::PUTCHAR, kernel::PUTCHAR),
        (sdk::POWER_OFF, kernel::POWER_OFF),
        (sdk::MAP_PAGE, kernel::MAP_PAGE),
        (sdk::BRK, kernel::BRK),
        (sdk::ADD_PORT, kernel::ADD_PORT),
        (sdk::G_ALLOC_PEN, kernel::GRAPHICS_ALLOC_PEN),
        (sdk::G_FREE_PEN, kernel::GRAPHICS_FREE_PEN),
        (sdk::G_SET_PEN, kernel::GRAPHICS_SET_PEN),
        (sdk::G_RECT, kernel::GRAPHICS_RECT),
        (sdk::G_FILL_RECT, kernel::GRAPHICS_FILL_RECT),
        (sdk::G_TEXT, kernel::GRAPHICS_TEXT),
        (sdk::G_LINE, kernel::GRAPHICS_LINE),
        (sdk::G_SET_CLIP, kernel::GRAPHICS_SET_CLIP),
        (sdk::G_PEN_COLOR, kernel::GRAPHICS_PEN_COLOR),
        (sdk::G_WRITE_PIXEL, kernel::GRAPHICS_WRITE_PIXEL),
        (sdk::G_MOVE, kernel::GRAPHICS_MOVE),
        (sdk::G_SET_PALETTE, kernel::GRAPHICS_SET_PALETTE),
        (sdk::G_RASTER_INFO, kernel::GRAPHICS_RASTER_INFO),
        (sdk::D_OPEN, kernel::DOS_OPEN),
    ];

    for (sdk_number, kernel_number) in pairs {
        assert_eq!(
            sdk_number, kernel_number,
            "SDK number {sdk_number} does not match the kernel's {kernel_number}"
        );
    }

    // The second half of the table, kept separate so the first pair list stays
    // readable rather than becoming a wall of names.
    let rest: [(u64, u64); 16] = [
        (sdk::L_CREATE, kernel::LAYERS_CREATE),
        (sdk::L_DESTROY, kernel::LAYERS_DESTROY),
        (sdk::L_SET_CLIP, kernel::LAYERS_SET_CLIP),
        (sdk::L_FRONT, kernel::LAYERS_FRONT),
        (sdk::L_BACK, kernel::LAYERS_BACK),
        (sdk::L_SET_VISIBLE, kernel::LAYERS_SET_VISIBLE),
        (sdk::L_MOVE, kernel::LAYERS_MOVE),
        (sdk::L_COMPOSITE, kernel::LAYERS_COMPOSITE),
        (sdk::L_ORDER, kernel::LAYERS_ORDER),
        (sdk::L_TOPMOST, kernel::LAYERS_TOPMOST),
        (sdk::I_OPEN_SCREEN, kernel::INTUITION_OPEN_SCREEN),
        (sdk::I_CLOSE_SCREEN, kernel::INTUITION_CLOSE_SCREEN),
        (sdk::I_OPEN_WINDOW, kernel::INTUITION_OPEN_WINDOW),
        (sdk::I_CLOSE_WINDOW, kernel::INTUITION_CLOSE_WINDOW),
        (sdk::I_SET_TITLE, kernel::INTUITION_SET_TITLE),
        (sdk::I_REFRESH, kernel::INTUITION_REFRESH),
    ];

    for (sdk_number, kernel_number) in rest {
        assert_eq!(
            sdk_number, kernel_number,
            "SDK number {sdk_number} does not match the kernel's {kernel_number}"
        );
    }

    // The remaining Intuition, DOS and utility numbers.
    let tail: [(u64, u64); 20] = [
        (sdk::I_SET_ATTRS, kernel::INTUITION_SET_ATTRS),
        (sdk::I_ADD_GADGET, kernel::INTUITION_ADD_GADGET),
        (sdk::I_REMOVE_GADGET, kernel::INTUITION_REMOVE_GADGET),
        (sdk::I_SET_BOUNDS, kernel::INTUITION_SET_BOUNDS),
        (sdk::I_FRONT, kernel::INTUITION_FRONT),
        (sdk::I_BACK, kernel::INTUITION_BACK),
        (sdk::I_REPAINT, kernel::INTUITION_REPAINT),
        (sdk::D_CLOSE, kernel::DOS_CLOSE),
        (sdk::D_READ, kernel::DOS_READ),
        (sdk::D_WRITE, kernel::DOS_WRITE),
        (sdk::D_SEEK, kernel::DOS_SEEK),
        (sdk::D_READ_DIR, kernel::DOS_READ_DIR),
        (sdk::D_DELETE, kernel::DOS_DELETE),
        (sdk::D_MKDIR, kernel::DOS_MKDIR),
        (sdk::D_EXAMINE, kernel::DOS_EXAMINE),
        (sdk::D_CURRENT_DIR, kernel::DOS_CURRENT_DIR),
        (sdk::D_EXAMINE_DIR, kernel::DOS_EXAMINE_DIR),
        (sdk::U_FIND_TAG, kernel::UTILITY_FIND_TAG),
        (sdk::U_STRNCPY, kernel::UTILITY_STRNCPY),
        (sdk::U_STRCMP, kernel::UTILITY_STRCMP),
    ];

    for (sdk_number, kernel_number) in tail {
        assert_eq!(
            sdk_number, kernel_number,
            "SDK number {sdk_number} does not match the kernel's {kernel_number}"
        );
    }

    // The icon and Workbench groups, added with their libraries.
    let icon: [(u64, u64); 21] = [
        (sdk::ICON_GET_DISK_OBJECT, kernel::ICON_GET_DISK_OBJECT),
        (
            sdk::ICON_GET_DISK_OBJECT_NEW,
            kernel::ICON_GET_DISK_OBJECT_NEW,
        ),
        (sdk::ICON_PUT_DISK_OBJECT, kernel::ICON_PUT_DISK_OBJECT),
        (sdk::ICON_FREE_DISK_OBJECT, kernel::ICON_FREE_DISK_OBJECT),
        (sdk::ICON_DUP_DISK_OBJECT, kernel::ICON_DUP_DISK_OBJECT),
        (
            sdk::ICON_DELETE_DISK_OBJECT,
            kernel::ICON_DELETE_DISK_OBJECT,
        ),
        (
            sdk::ICON_GET_DEF_DISK_OBJECT,
            kernel::ICON_GET_DEF_DISK_OBJECT,
        ),
        (
            sdk::ICON_PUT_DEF_DISK_OBJECT,
            kernel::ICON_PUT_DEF_DISK_OBJECT,
        ),
        (sdk::ICON_NEW_DISK_OBJECT, kernel::ICON_NEW_DISK_OBJECT),
        (sdk::ICON_FIND_TOOL_TYPE, kernel::ICON_FIND_TOOL_TYPE),
        (sdk::ICON_MATCH_TOOL_VALUE, kernel::ICON_MATCH_TOOL_VALUE),
        (sdk::ICON_GET_ICON_TAG_LIST, kernel::ICON_GET_ICON_TAG_LIST),
        (sdk::ICON_PUT_ICON_TAG_LIST, kernel::ICON_PUT_ICON_TAG_LIST),
        (sdk::ICON_BUMP_REVISION, kernel::ICON_BUMP_REVISION),
        (sdk::ICON_LAYOUT_ICON, kernel::ICON_LAYOUT_ICON),
        (
            sdk::ICON_GET_ICON_RECTANGLE,
            kernel::ICON_GET_ICON_RECTANGLE,
        ),
        (sdk::ICON_ICON_CONTROL, kernel::ICON_ICON_CONTROL),
        (sdk::ICON_DRAW_ICON_STATE, kernel::ICON_DRAW_ICON_STATE),
        (sdk::ICON_ADD_FREE_LIST, kernel::ICON_ADD_FREE_LIST),
        (sdk::ICON_FREE_FREE_LIST, kernel::ICON_FREE_FREE_LIST),
        (
            sdk::ICON_CHANGE_TO_SELECTED_ICON_COLOR,
            kernel::ICON_CHANGE_TO_SELECTED_ICON_COLOR,
        ),
    ];

    for (sdk_number, kernel_number) in icon {
        assert_eq!(
            sdk_number, kernel_number,
            "SDK number {sdk_number} does not match the kernel's {kernel_number}"
        );
    }

    let workbench: [(u64, u64); 22] = [
        (sdk::WORKBENCH_ADD_APP_ICON, kernel::WORKBENCH_ADD_APP_ICON),
        (
            sdk::WORKBENCH_ADD_APP_MENU_ITEM,
            kernel::WORKBENCH_ADD_APP_MENU_ITEM,
        ),
        (
            sdk::WORKBENCH_ADD_APP_WINDOW,
            kernel::WORKBENCH_ADD_APP_WINDOW,
        ),
        (
            sdk::WORKBENCH_ADD_APP_WINDOW_DROP_ZONE,
            kernel::WORKBENCH_ADD_APP_WINDOW_DROP_ZONE,
        ),
        (
            sdk::WORKBENCH_CLOSE_WORKBENCH_OBJECT,
            kernel::WORKBENCH_CLOSE_WORKBENCH_OBJECT,
        ),
        (
            sdk::WORKBENCH_GET_NEXT_APP_ICON,
            kernel::WORKBENCH_GET_NEXT_APP_ICON,
        ),
        (
            sdk::WORKBENCH_MAKE_WORKBENCH_OBJECT_VISIBLE,
            kernel::WORKBENCH_MAKE_WORKBENCH_OBJECT_VISIBLE,
        ),
        (
            sdk::WORKBENCH_NOTIFY_WORKBENCH,
            kernel::WORKBENCH_NOTIFY_WORKBENCH,
        ),
        (
            sdk::WORKBENCH_OPEN_WORKBENCH_OBJECT,
            kernel::WORKBENCH_OPEN_WORKBENCH_OBJECT,
        ),
        (
            sdk::WORKBENCH_QUOTE_WORKBENCH,
            kernel::WORKBENCH_QUOTE_WORKBENCH,
        ),
        (
            sdk::WORKBENCH_REGISTER_WORKBENCH,
            kernel::WORKBENCH_REGISTER_WORKBENCH,
        ),
        (
            sdk::WORKBENCH_REMOVE_APP_ICON,
            kernel::WORKBENCH_REMOVE_APP_ICON,
        ),
        (
            sdk::WORKBENCH_REMOVE_APP_MENU_ITEM,
            kernel::WORKBENCH_REMOVE_APP_MENU_ITEM,
        ),
        (
            sdk::WORKBENCH_REMOVE_APP_WINDOW,
            kernel::WORKBENCH_REMOVE_APP_WINDOW,
        ),
        (
            sdk::WORKBENCH_REMOVE_APP_WINDOW_DROP_ZONE,
            kernel::WORKBENCH_REMOVE_APP_WINDOW_DROP_ZONE,
        ),
        (
            sdk::WORKBENCH_SEND_APP_WINDOW_MESSAGE,
            kernel::WORKBENCH_SEND_APP_WINDOW_MESSAGE,
        ),
        (
            sdk::WORKBENCH_START_WORKBENCH,
            kernel::WORKBENCH_START_WORKBENCH,
        ),
        (
            sdk::WORKBENCH_UNREGISTER_WORKBENCH,
            kernel::WORKBENCH_UNREGISTER_WORKBENCH,
        ),
        (
            sdk::WORKBENCH_UPDATE_WORKBENCH_OBJECT,
            kernel::WORKBENCH_UPDATE_WORKBENCH_OBJECT,
        ),
        (sdk::WORKBENCH_WB_CONFIG, kernel::WORKBENCH_WB_CONFIG),
        (sdk::WORKBENCH_WB_INFO, kernel::WORKBENCH_WB_INFO),
        (
            sdk::WORKBENCH_WORKBENCH_CONTROL,
            kernel::WORKBENCH_WORKBENCH_CONTROL,
        ),
    ];

    for (sdk_number, kernel_number) in workbench {
        assert_eq!(
            sdk_number, kernel_number,
            "SDK number {sdk_number} does not match the kernel's {kernel_number}"
        );
    }
}

/// Every error code the SDK publishes is the kernel's code.
#[test]
fn the_sdk_error_codes_match_the_kernel_error_codes() {
    let pairs: [(i32, i32); 21] = [
        (sdk::errors::OK, kernel::errors::OK),
        (
            sdk::errors::NOT_IMPLEMENTED,
            kernel::errors::NOT_IMPLEMENTED,
        ),
        (sdk::errors::NO_OBJECT, kernel::errors::NO_OBJECT),
        (sdk::errors::OBJECT_IN_USE, kernel::errors::OBJECT_IN_USE),
        (sdk::errors::INVALID_NAME, kernel::errors::INVALID_NAME),
        (sdk::errors::NOT_FOUND, kernel::errors::NOT_FOUND),
        (sdk::errors::PATH_NOT_FOUND, kernel::errors::PATH_NOT_FOUND),
        (sdk::errors::TOO_LARGE, kernel::errors::TOO_LARGE),
        (sdk::errors::NO_MEMORY, kernel::errors::NO_MEMORY),
        (sdk::errors::BAD_RECT, kernel::errors::BAD_RECT),
        (
            sdk::errors::FILE_NOT_OBJECT,
            kernel::errors::FILE_NOT_OBJECT,
        ),
        (sdk::errors::RANGE, kernel::errors::RANGE),
        (sdk::errors::NOT_OWNER, kernel::errors::NOT_OWNER),
        (sdk::errors::INVALID_SIGNAL, kernel::errors::INVALID_SIGNAL),
        (sdk::errors::NO_REPLY_PORT, kernel::errors::NO_REPLY_PORT),
        (sdk::errors::NO_MESSAGE, kernel::errors::NO_MESSAGE),
        (sdk::errors::TASK_NOT_FOUND, kernel::errors::TASK_NOT_FOUND),
        (sdk::errors::BAD_POINTER, kernel::errors::BAD_POINTER),
        (sdk::errors::BAD_RANGE, kernel::errors::BAD_RANGE),
        (sdk::errors::BAD_STRING, kernel::errors::BAD_STRING),
        (sdk::errors::NO_PORT, kernel::errors::NO_PORT),
    ];

    for (sdk_code, kernel_code) in pairs {
        assert_eq!(
            sdk_code, kernel_code,
            "SDK error {sdk_code} does not match the kernel's {kernel_code}"
        );
    }

    // The page size is the granularity both `brk` and the SDK use.
    assert_eq!(sdk::PAGE_SIZE as u64, PAGE_SIZE);
}
