//! Layout and big-endian round-trip tests for the guest32 (68k) ABI.
//!
//! # What is being tested
//!
//! The `amiga/abi-guest32` crate is still a placeholder, so these tests
//! exercise the reference implementation carried in this crate's library
//! (`vela_test_abi_layout`). They assert two separate things:
//!
//! 1. **Layout** — the in-memory offsets of each 68k structure, which is
//!    what a 68k program reads out of memory.
//! 2. **Wire byte order** — that the serialised form is big-endian, and that
//!    decoding it reproduces the original value exactly.
//!
//! # Why the wire form exists at all
//!
//! A 68k program reads its structures directly out of memory, so on a real
//! 68k system the memory image *is* the wire format. VelaOS's guest bridge
//! has to produce that image from a host that is almost certainly
//! little-endian, so the encoding is written out explicitly rather than
//! being whatever the host's memory layout happens to be.
//!
//! The tests therefore assert that the wire bytes differ from the in-memory
//! bytes on a little-endian host. If they did not differ, the codec would
//! be doing nothing and the guest would read every multi-byte scalar
//! backwards.

use core::mem::{align_of, offset_of, size_of};

use vela_test_abi_layout::{
    bstr_bytes, bstr_from_bytes, GuestBptr32, GuestList32, GuestMessage32, GuestMsgPort32,
    GuestNode32, GuestPtr32, GuestTagItem32, WireError,
};

/// The 68k `Node` layout.
#[test]
fn the_guest_node_layout_is_stable() {
    assert_eq!(size_of::<GuestNode32>(), 20);
    assert_eq!(align_of::<GuestNode32>(), 4);
    assert_eq!(offset_of!(GuestNode32, ln_type), 0);
    assert_eq!(offset_of!(GuestNode32, ln_pri), 1);
    assert_eq!(offset_of!(GuestNode32, ln_name), 2);
    assert_eq!(offset_of!(GuestNode32, ln_next), 12);
    assert_eq!(offset_of!(GuestNode32, ln_prev), 16);
}

/// The 68k `List` layout.
#[test]
fn the_guest_list_layout_is_stable() {
    assert_eq!(size_of::<GuestList32>(), 12);
    assert_eq!(align_of::<GuestList32>(), 4);
    assert_eq!(offset_of!(GuestList32, lh_head), 0);
    assert_eq!(offset_of!(GuestList32, lh_tail), 4);
    assert_eq!(offset_of!(GuestList32, lh_chain), 8);
}

/// The 68k `TagItem` layout.
#[test]
fn the_guest_tag_item_layout_is_stable() {
    assert_eq!(size_of::<GuestTagItem32>(), 8);
    assert_eq!(align_of::<GuestTagItem32>(), 4);
    assert_eq!(offset_of!(GuestTagItem32, ti_tag), 0);
    assert_eq!(offset_of!(GuestTagItem32, ti_data), 4);
}

/// The 68k `MsgPort` layout.
#[test]
fn the_guest_msg_port_layout_is_stable() {
    assert_eq!(size_of::<GuestMsgPort32>(), 36);
    assert_eq!(align_of::<GuestMsgPort32>(), 4);
    assert_eq!(offset_of!(GuestMsgPort32, mp_node), 0);
    assert_eq!(offset_of!(GuestMsgPort32, mp_sig), 20);
    assert_eq!(offset_of!(GuestMsgPort32, mp_flags), 21);
    assert_eq!(offset_of!(GuestMsgPort32, mp_sig_wait), 22);
    assert_eq!(offset_of!(GuestMsgPort32, mp_reserved), 23);
    assert_eq!(offset_of!(GuestMsgPort32, mp_left), 24);
    assert_eq!(offset_of!(GuestMsgPort32, mp_hide), 26);
    assert_eq!(offset_of!(GuestMsgPort32, mp_reply_port), 28);
    assert_eq!(offset_of!(GuestMsgPort32, mp_user_data), 32);
}

/// The 68k `Message` layout.
#[test]
fn the_guest_message_layout_is_stable() {
    assert_eq!(size_of::<GuestMessage32>(), 28);
    assert_eq!(align_of::<GuestMsgPort32>(), 4);
    assert_eq!(offset_of!(GuestMessage32, mn_reply_port), 0);
    assert_eq!(offset_of!(GuestMessage32, mn_length), 4);
    assert_eq!(offset_of!(GuestMessage32, mn_reserved), 6);
    assert_eq!(offset_of!(GuestMessage32, mn_node), 8);
}

/// A guest pointer is a 32-bit offset, not a host pointer.
#[test]
fn a_guest_pointer_is_four_bytes() {
    assert_eq!(size_of::<GuestPtr32>(), 4);
    assert_eq!(align_of::<GuestPtr32>(), 4);
    assert!(GuestPtr32::NULL.is_null());
    assert!(!GuestPtr32(1).is_null());
}

/// A node's wire form is big-endian.
///
/// The link values are the check: `0x01020304` must appear as
/// `01 02 03 04`, most significant byte first. On a little-endian host the
/// in-memory bytes would be `04 03 02 01`, so this also proves the codec is
/// actually doing something.
#[test]
fn a_node_is_encoded_big_endian() {
    let node = GuestNode32 {
        ln_type: 3,
        ln_pri: -1,
        ln_name: [b'a'; GuestNode32::NAME_LEN],
        ln_next: GuestPtr32(0x0102_0304),
        ln_prev: GuestPtr32(0xA0B0_C0D0),
    };
    let wire = node.to_wire();
    assert_eq!(wire.len(), GuestNode32::SIZE);
    assert_eq!(wire[0], 3);
    assert_eq!(wire[1], 0xFF, "the priority is a signed byte");
    assert_eq!(&wire[12..16], &[0x01, 0x02, 0x03, 0x04]);
    assert_eq!(&wire[16..20], &[0xA0, 0xB0, 0xC0, 0xD0]);
}

/// The wire bytes differ from the in-memory bytes on a little-endian host.
///
/// This is the check that catches a codec that quietly became a memcpy.
#[test]
fn the_wire_form_differs_from_the_host_memory_form() {
    let node = GuestNode32 {
        ln_type: 1,
        ln_pri: 0,
        ln_name: [0; GuestNode32::NAME_LEN],
        ln_next: GuestPtr32(0x0102_0304),
        ln_prev: GuestPtr32::NULL,
    };
    let wire = node.to_wire();
    let mut host = Vec::with_capacity(GuestNode32::SIZE);
    host.push(node.ln_type);
    host.push(node.ln_pri as u8);
    host.extend_from_slice(&node.ln_name);
    host.extend_from_slice(&node.ln_next.0.to_ne_bytes());
    host.extend_from_slice(&node.ln_prev.0.to_ne_bytes());
    assert_ne!(
        wire, host,
        "the guest wire form must not be the host's native byte order"
    );
}

/// A node round-trips through the wire form.
#[test]
fn a_node_round_trips() {
    let node = GuestNode32::new(3, -5, "hello");
    let mut node = node;
    node.ln_next = GuestPtr32(0xDEAD_BEEF);
    node.ln_prev = GuestPtr32(0x1234_5678);
    let wire = node.to_wire();
    assert_eq!(GuestNode32::from_wire(&wire).unwrap(), node);
}

/// A list round-trips through the wire form.
#[test]
fn a_list_round_trips() {
    let list = GuestList32 {
        lh_head: GuestPtr32(0x1000),
        lh_tail: GuestPtr32(0x2000),
        lh_chain: GuestPtr32(0x3000),
    };
    let wire = list.to_wire();
    assert_eq!(wire.len(), GuestList32::SIZE);
    assert_eq!(&wire[0..4], &[0x00, 0x00, 0x10, 0x00]);
    assert_eq!(GuestList32::from_wire(&wire).unwrap(), list);
}

/// A tag round-trips through the wire form.
#[test]
fn a_tag_round_trips() {
    let tag = GuestTagItem32 {
        ti_tag: 0x8000_0001,
        ti_data: 0xFFFF_FFFF,
    };
    let wire = tag.to_wire();
    assert_eq!(&wire[0..4], &[0x80, 0x00, 0x00, 0x01]);
    assert_eq!(&wire[4..8], &[0xFF, 0xFF, 0xFF, 0xFF]);
    assert_eq!(GuestTagItem32::from_wire(&wire).unwrap(), tag);
}

/// A port round-trips through the wire form.
#[test]
fn a_port_round_trips() {
    let port = GuestMsgPort32 {
        mp_node: GuestNode32::new(1, 0, "port"),
        mp_sig: 0x80,
        mp_flags: 0x03,
        mp_sig_wait: 0x40,
        mp_reserved: 0,
        mp_left: 5,
        mp_hide: 2,
        mp_reply_port: GuestPtr32(0xABCD_1234),
        mp_user_data: GuestPtr32(0x0BAD_F00D),
    };
    let wire = port.to_wire();
    assert_eq!(wire.len(), GuestMsgPort32::SIZE);
    assert_eq!(GuestMsgPort32::from_wire(&wire).unwrap(), port);
}

/// A message round-trips through the wire form.
#[test]
fn a_message_round_trips() {
    let message = GuestMessage32 {
        mn_reply_port: GuestPtr32(0x0BAD_F00D),
        mn_length: 28,
        mn_reserved: 0,
        mn_node: GuestNode32::new(2, 0, "msg"),
    };
    let wire = message.to_wire();
    assert_eq!(wire.len(), GuestMessage32::SIZE);
    assert_eq!(&wire[4..6], &[0x00, 0x1C], "the length is big-endian");
    assert_eq!(GuestMessage32::from_wire(&wire).unwrap(), message);
}

/// A truncated buffer is a clean error, not a panic and not a read past the
/// end.
#[test]
fn a_truncated_buffer_is_a_clean_error() {
    let node = GuestNode32::new(1, 0, "x");
    let wire = node.to_wire();
    for len in 0..GuestNode32::SIZE {
        let err = GuestNode32::from_wire(&wire[..len]).unwrap_err();
        assert_eq!(
            err,
            WireError::TooShort {
                needed: GuestNode32::SIZE,
                got: len
            }
        );
    }
}

/// A buffer longer than the structure is accepted: the extra bytes belong
/// to whatever follows the structure.
#[test]
fn a_longer_buffer_is_accepted() {
    let mut wire = GuestNode32::new(1, 0, "x").to_wire();
    wire.extend_from_slice(b"trailing");
    assert!(GuestNode32::from_wire(&wire).is_ok());
}

/// The 68k `BPTR` is a shifted address, unlike the native64 opaque handle.
#[test]
fn a_guest_bptr_is_a_shifted_address() {
    let handle = GuestBptr32::from_address(0x1234_5678);
    assert_eq!(handle.baddr(), 0x1234_5678 >> 2);
    assert_eq!(handle.address(), 0x1234_5678);
    assert!(GuestBptr32::NULL.is_null());
    assert!(!handle.is_null());
}

/// An odd address cannot be a `BPTR`, because the shift would not
/// round-trip.
#[test]
#[should_panic(expected = "four-byte-aligned")]
fn an_odd_address_is_rejected_by_the_guest_bptr() {
    let _ = GuestBptr32::from_address(0x1234_5679);
}

/// A `BSTR` is a length byte followed by that many characters.
#[test]
fn a_bstr_round_trips() {
    let bytes = bstr_bytes("Ram:Disk");
    assert_eq!(bytes[0], 8);
    assert_eq!(&bytes[1..], b"Ram:Disk");
    assert_eq!(bstr_from_bytes(&bytes).unwrap(), "Ram:Disk");
}

/// An empty string is a zero length byte and no text.
#[test]
fn an_empty_bstr_round_trips() {
    let bytes = bstr_bytes("");
    assert_eq!(bytes, vec![0u8]);
    assert_eq!(bstr_from_bytes(&bytes).unwrap(), "");
}

/// A truncated `BSTR` is rejected rather than read past the end.
#[test]
fn a_truncated_bstr_is_rejected() {
    assert!(bstr_from_bytes(&[8, b'a', b'b']).is_none());
    assert!(bstr_from_bytes(&[]).is_none());
}

/// A `BSTR` whose length byte exceeds its text is rejected.
#[test]
fn an_overlong_bstr_length_is_rejected() {
    let mut bytes = bstr_bytes("abc");
    bytes[0] = 200;
    assert!(bstr_from_bytes(&bytes).is_none());
}
