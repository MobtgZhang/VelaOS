//! Layout tests for the native64 ABI.
//!
//! Every assertion here is an ABI guarantee: the offsets are baked into the
//! SDK's generated headers, the kernel's copies of the structures and the
//! hosted test harness. Changing one of them without bumping the ABI
//! version is a silent break, and a silent break in a shared structure is
//! the kind that shows up as a corrupted field three subsystems away.
//!
//! These assertions deliberately duplicate the ones inside
//! `amiga/abi-native64` itself. That is intentional: the crate's own tests
//! protect the crate, and these protect the *contract* — the layout the
//! design document specifies — from a change that updated the crate and its
//! tests together but broke the published ABI.

use core::mem::{align_of, offset_of, size_of};

use vela_abi_native64::{
    AnchorPath, Bptr, DateStamp, FileInfo, IntuiMessage, LibraryHandle, List, Lvo, Message,
    MsgPort, Node, TagItem, ANCHOR_PATH_BUF_LEN, FILE_INFO_COMMENT_LEN, LIBRARY_VECTOR_SLOTS,
    MSG_PAYLOAD_WORDS, NODE_NAME_LEN, POINTER_SIZE,
};

/// The `Node` layout: the head of every AmigaOS-style list.
#[test]
fn the_node_layout_is_stable() {
    assert_eq!(size_of::<Node>(), 32);
    assert_eq!(align_of::<Node>(), 8);
    assert_eq!(offset_of!(Node, ln_type), 0);
    assert_eq!(offset_of!(Node, ln_pri), 1);
    assert_eq!(offset_of!(Node, ln_name), 2);
    // The links are 8-byte aligned, which puts them four bytes past the end
    // of the 12-byte head.
    assert_eq!(offset_of!(Node, ln_next), 16);
    assert_eq!(offset_of!(Node, ln_prev), 24);
}

/// The `List` layout: a doubly-linked list head.
#[test]
fn the_list_layout_is_stable() {
    assert_eq!(size_of::<List>(), 16);
    assert_eq!(align_of::<List>(), 8);
    assert_eq!(offset_of!(List, lh_head), 0);
    assert_eq!(offset_of!(List, lh_tail), 8);
}

/// The `MsgPort` layout: a queue of messages plus a signal mask.
#[test]
fn the_msg_port_layout_is_stable() {
    assert_eq!(size_of::<MsgPort>(), 80);
    assert_eq!(align_of::<MsgPort>(), 8);
    assert_eq!(offset_of!(MsgPort, mp_node), 0);
    assert_eq!(offset_of!(MsgPort, mp_sig), 32);
    assert_eq!(offset_of!(MsgPort, mp_sig_wait), 36);
    assert_eq!(offset_of!(MsgPort, mp_flags), 40);
    assert_eq!(offset_of!(MsgPort, mp_port), 48);
    assert_eq!(offset_of!(MsgPort, mp_private), 56);
    assert_eq!(offset_of!(MsgPort, mp_owner), 64);
    assert_eq!(offset_of!(MsgPort, mp_msg), 72);
}

/// The `Message` layout: the core of the Exec message system.
#[test]
fn the_message_layout_is_stable() {
    assert_eq!(size_of::<Message>(), 104);
    assert_eq!(align_of::<Message>(), 8);
    assert_eq!(offset_of!(Message, mn_reply_port), 0);
    assert_eq!(offset_of!(Message, mn_node), 8);
    assert_eq!(offset_of!(Message, mn_sig), 40);
    assert_eq!(offset_of!(Message, mn_reply), 48);
    assert_eq!(offset_of!(Message, mn_payload), 56);
}

/// The `TagItem` layout: a typed key/value pair.
#[test]
fn the_tag_item_layout_is_stable() {
    assert_eq!(size_of::<TagItem>(), 16);
    assert_eq!(align_of::<TagItem>(), 8);
    assert_eq!(offset_of!(TagItem, ti_Tag), 0);
    assert_eq!(offset_of!(TagItem, ti_Data), 8);
}

/// The `IntuiMessage` layout: an Intuition event as a program sees it.
#[test]
fn the_intui_message_layout_is_stable() {
    assert_eq!(size_of::<IntuiMessage>(), 32);
    assert_eq!(align_of::<IntuiMessage>(), 8);
    assert_eq!(offset_of!(IntuiMessage, im_Type), 0);
    assert_eq!(offset_of!(IntuiMessage, im_Code), 2);
    assert_eq!(offset_of!(IntuiMessage, im_Source), 8);
    assert_eq!(offset_of!(IntuiMessage, im_MouseX), 16);
    assert_eq!(offset_of!(IntuiMessage, im_MouseY), 18);
    assert_eq!(offset_of!(IntuiMessage, im_Button), 20);
    assert_eq!(offset_of!(IntuiMessage, im_Flags), 22);
    assert_eq!(offset_of!(IntuiMessage, im_sig), 24);
}

/// The `DateStamp` layout: AmigaOS's timestamp.
#[test]
fn the_date_stamp_layout_is_stable() {
    assert_eq!(size_of::<DateStamp>(), 12);
    assert_eq!(align_of::<DateStamp>(), 4);
    assert_eq!(offset_of!(DateStamp, ds_Days), 0);
    assert_eq!(offset_of!(DateStamp, ds_Minute), 4);
    assert_eq!(offset_of!(DateStamp, ds_Tick), 8);
}

/// The `FileInfo` layout: what `Examine` returns.
#[test]
fn the_file_info_layout_is_stable() {
    assert_eq!(size_of::<FileInfo>(), 184);
    assert_eq!(align_of::<FileInfo>(), 8);
    assert_eq!(offset_of!(FileInfo, fi_EntryType), 0);
    assert_eq!(offset_of!(FileInfo, fi_EntryKey), 4);
    assert_eq!(offset_of!(FileInfo, fi_Size), 8);
    assert_eq!(offset_of!(FileInfo, fi_DiskKey), 16);
    assert_eq!(offset_of!(FileInfo, fi_Protection), 20);
    assert_eq!(offset_of!(FileInfo, fi_OwnerUID), 24);
    assert_eq!(offset_of!(FileInfo, fi_OwnerGID), 28);
    assert_eq!(offset_of!(FileInfo, fi_CommentLen), 32);
    assert_eq!(offset_of!(FileInfo, fi_Time), 40);
    assert_eq!(offset_of!(FileInfo, fi_OwnerUID64), 56);
    assert_eq!(offset_of!(FileInfo, fi_OwnerGID64), 64);
    assert_eq!(offset_of!(FileInfo, fi_CommentLen2), 72);
    assert_eq!(offset_of!(FileInfo, fi_Comment), 76);
}

/// The `AnchorPath` layout: what `ExamineDir` fills in.
#[test]
fn the_anchor_path_layout_is_stable() {
    assert_eq!(size_of::<AnchorPath>(), 276);
    assert_eq!(align_of::<AnchorPath>(), 4);
    assert_eq!(offset_of!(AnchorPath, ap_First), 0);
    assert_eq!(offset_of!(AnchorPath, ap_FoundBreak), 1);
    assert_eq!(offset_of!(AnchorPath, ap_Flags), 2);
    assert_eq!(offset_of!(AnchorPath, ap_Reserved), 3);
    assert_eq!(offset_of!(AnchorPath, ap_Strlen), 4);
    assert_eq!(offset_of!(AnchorPath, ap_SearchFlags), 8);
    assert_eq!(offset_of!(AnchorPath, ap_SegListBlock), 12);
    assert_eq!(offset_of!(AnchorPath, ap_Reserved1), 16);
    assert_eq!(offset_of!(AnchorPath, ap_Buf), 20);
}

/// The handle types are all transparent, so their size is the size of the
/// integer they wrap.
#[test]
fn the_handle_types_are_transparent() {
    assert_eq!(size_of::<Bptr>(), 4);
    assert_eq!(align_of::<Bptr>(), 4);
    assert_eq!(size_of::<LibraryHandle>(), 4);
    assert_eq!(align_of::<LibraryHandle>(), 4);
    assert_eq!(size_of::<Lvo>(), 2);
    assert_eq!(align_of::<Lvo>(), 2);
}

/// The `FileInfo`'s inline comment buffer has the length the layout assumes.
#[test]
fn the_file_info_comment_buffer_has_the_documented_length() {
    assert_eq!(FILE_INFO_COMMENT_LEN, 108);
    assert_eq!(
        offset_of!(FileInfo, fi_Comment) + FILE_INFO_COMMENT_LEN,
        184
    );
}

/// The `AnchorPath`'s inline path buffer has the length the layout assumes.
#[test]
fn the_anchor_path_buffer_has_the_documented_length() {
    assert_eq!(ANCHOR_PATH_BUF_LEN, 256);
    assert_eq!(offset_of!(AnchorPath, ap_Buf) + ANCHOR_PATH_BUF_LEN, 276);
}

/// The constants the layout depends on have not drifted.
#[test]
fn the_layout_constants_are_consistent() {
    assert_eq!(POINTER_SIZE, 8);
    assert_eq!(NODE_NAME_LEN, 10);
    assert_eq!(MSG_PAYLOAD_WORDS, 6);
    // The jump table's last slot sits at `-POINTER_SIZE`, so the geometry
    // has to survive a change of the pointer size.
    assert_eq!(LIBRARY_VECTOR_SLOTS, 512);
    assert_eq!(
        Lvo::new(-(LIBRARY_VECTOR_SLOTS as i16 * POINTER_SIZE as i16)).index(),
        511
    );
}
