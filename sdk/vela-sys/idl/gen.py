#!/usr/bin/env python3
"""One-off helper: emit the SDK's IDL YAML files from a single table.

The table below is the whole of the SDK's library surface: the AmigaOS
function name, its library-vector offset, the Amiga-style constant suffix,
the generated stub's name, and the syscall the stub reaches.

Re-run this after editing the table; the IDL is what the SDK's build script
actually reads, and the build script checks what it finds there against the
SDK's own `numbers.rs`.

    python3 sdk/vela-sys/idl/gen.py
"""

# (module, library, prefix, [(function, lvo, suffix, stub, syscall, number)])
LIBRARIES = [
    ("exec", "exec.library", "EXEC", [
        ("MapPage", -168, "MAPPAGE", "map_page", "MAP_PAGE", 18),
        ("Brk", -160, "BRK", "brk", "BRK", 19),
        ("PutChar", -152, "PUTCHAR", "put_char", "PUTCHAR", 16),
        ("PowerOff", -144, "POWEROFF", "power_off", "POWER_OFF", 17),
        ("CreateMsgPort", -136, "CREATEMSGPORT", "create_port", "CREATE_MSG_PORT", 0),
        ("DeleteMsgPort", -128, "DELETEMSGPORT", "delete_port", "DELETE_MSG_PORT", 1),
        ("AddPort", -120, "ADDPORT", "add_port", None, None),
        ("Forbid", -112, "FORBID", "forbid", "FORBID", 12),
        ("Permit", -104, "PERMIT", "permit", "PERMIT", 13),
        ("PutMsg", -96, "PUTMSG", "put_msg", "PUT_MSG", 2),
        ("GetMsg", -88, "GETMSG", "get_msg", "GET_MSG", 3),
        ("ReplyMsg", -80, "REPLYMSG", "reply_msg", "REPLY_MSG", 4),
        ("WaitPort", -72, "WAITPORT", "wait_port", "WAIT_PORT", 5),
        ("Wait", -64, "WAIT", "wait", "WAIT", 6),
        ("SetSignal", -56, "SETSIGNAL", "set_signal", "SET_SIGNAL", 7),
        ("SetExcept", -48, "SETEXCEPT", "set_except", "SET_EXCEPT", 8),
        ("Signal", -40, "SIGNAL", "signal", "SIGNAL", 9),
        ("OpenLibrary", -32, "OPENLIBRARY", "open_library", "OPEN_LIBRARY", 10),
        ("CloseLibrary", -24, "CLOSELIBRARY", "close_library", "CLOSE_LIBRARY", 11),
        ("Yield", -16, "YIELD", "yield_now", "YIELD", 14),
        ("FindTask", -8, "FINDTASK", "find_task", "GET_TASK_ID", 15),
    ]),
    ("graphics", "graphics.library", "GRAPHICS", [
        ("AllocPen", -104, "ALLOCPEN", "alloc_pen", "G_ALLOC_PEN", 64),
        ("FreePen", -96, "FREEPEN", "free_pen", "G_FREE_PEN", 65),
        ("SetPen", -88, "SETPEN", "set_pen", "G_SET_PEN", 66),
        ("Rect", -80, "RECT", "rect", "G_RECT", 67),
        ("RectFill", -72, "RECTFILL", "fill_rect", "G_FILL_RECT", 68),
        ("Text", -64, "TEXT", "text", "G_TEXT", 69),
        ("Line", -56, "LINE", "line", "G_LINE", 70),
        ("SetClip", -48, "SETCLIP", "set_clip", "G_SET_CLIP", 71),
        ("PenColor", -40, "PENCOLOR", "pen_color", "G_PEN_COLOR", 72),
        ("WritePixel", -32, "WRITEPIXEL", "write_pixel", "G_WRITE_PIXEL", 73),
        ("Move", -24, "MOVE", "move_to", "G_MOVE", 74),
        ("SetPalette", -16, "SETPALETTE", "set_palette", "G_SET_PALETTE", 75),
        ("RasterInfo", -8, "RASTERINFO", "raster_info", "G_RASTER_INFO", 76),
    ]),
    ("intuition", "intuition.library", "INTUITION", [
        ("OpenScreen", -104, "OPENSCREEN", "open_screen", "I_OPEN_SCREEN", 192),
        ("CloseScreen", -96, "CLOSESCREEN", "close_screen", "I_CLOSE_SCREEN", 193),
        ("OpenWindow", -88, "OPENWINDOW", "open_window", "I_OPEN_WINDOW", 194),
        ("CloseWindow", -80, "CLOSEWINDOW", "close_window", "I_CLOSE_WINDOW", 195),
        ("SetWindowTitle", -72, "SETWINDOWTITLE", "set_title", "I_SET_TITLE", 196),
        ("Refresh", -64, "REFRESH", "refresh", "I_REFRESH", 197),
        ("SetAttrs", -56, "SETATTRS", "set_attrs", "I_SET_ATTRS", 198),
        ("AddGadget", -48, "ADDGADGET", "add_gadget", "I_ADD_GADGET", 199),
        ("RemoveGadget", -40, "REMOVEGADGET", "remove_gadget", "I_REMOVE_GADGET", 200),
        ("SetWindowBounds", -32, "SETWINDOWBOUNDS", "set_bounds", "I_SET_BOUNDS", 201),
        ("BringWindowToFront", -24, "BRINGWINDOWTOFRONT", "to_front", "I_FRONT", 202),
        ("SendWindowToBack", -16, "SENDWINDOWTOBACK", "to_back", "I_BACK", 203),
        ("RepaintWindow", -8, "REPAINTWINDOW", "repaint", "I_REPAINT", 204),
    ]),
    ("layers", "layers.library", "LAYERS", [
        ("CreateLayer", -80, "CREATELAYER", "create_layer", "L_CREATE", 128),
        ("DeleteLayer", -72, "DELETELAYER", "delete_layer", "L_DESTROY", 129),
        ("SetClip", -64, "SETCLIP", "set_clip", "L_SET_CLIP", 130),
        ("BringToFront", -56, "BRINGTOFRONT", "bring_to_front", "L_FRONT", 131),
        ("SendToBack", -48, "SENDTOBACK", "send_to_back", "L_BACK", 132),
        ("SetVisible", -40, "SETVISIBLE", "set_visible", "L_SET_VISIBLE", 133),
        ("MoveLayer", -32, "MOVELAYER", "move_layer", "L_MOVE", 134),
        ("Composite", -24, "COMPOSITE", "composite", "L_COMPOSITE", 135),
        ("ObtainOrder", -16, "OBTAINORDER", "order", "L_ORDER", 136),
        ("WhichLayer", -8, "WHICHLAYER", "topmost", "L_TOPMOST", 137),
    ]),
    ("dos", "dos.library", "DOS", [
        ("Open", -80, "OPEN", "open", "D_OPEN", 256),
        ("Close", -72, "CLOSE", "close", "D_CLOSE", 257),
        ("Read", -64, "READ", "read", "D_READ", 258),
        ("Write", -56, "WRITE", "write", "D_WRITE", 259),
        ("Seek", -48, "SEEK", "seek", "D_SEEK", 260),
        ("ReadDir", -40, "READDIR", "read_dir", "D_READ_DIR", 261),
        ("DeleteFile", -32, "DELETEFILE", "delete", "D_DELETE", 262),
        ("CreateDir", -24, "CREATEDIR", "mkdir", "D_MKDIR", 263),
        ("Examine", -16, "EXAMINE", "examine", "D_EXAMINE", 264),
        ("CurrentDir", -8, "CURRENTDIR", "current_dir", "D_CURRENT_DIR", 265),
    ]),
]

# The kernel's name for each syscall, for the conformance document. The build
# script checks the number against the SDK's own `numbers.rs`; the hosted
# conformance suite checks the number against the kernel's.
KERNEL_NAMES = {
    "MAP_PAGE": "MAP_PAGE",
    "BRK": "BRK",
    "PUTCHAR": "PUTCHAR",
    "POWER_OFF": "POWER_OFF",
    "CREATE_MSG_PORT": "CREATE_MSG_PORT",
    "DELETE_MSG_PORT": "DELETE_MSG_PORT",
    "FORBID": "FORBID",
    "PERMIT": "PERMIT",
    "PUT_MSG": "PUT_MSG",
    "GET_MSG": "GET_MSG",
    "REPLY_MSG": "REPLY_MSG",
    "WAIT_PORT": "WAIT_PORT",
    "WAIT": "WAIT",
    "SET_SIGNAL": "SET_SIGNAL",
    "SET_EXCEPT": "SET_EXCEPT",
    "SIGNAL": "SIGNAL",
    "OPEN_LIBRARY": "OPEN_LIBRARY",
    "CLOSE_LIBRARY": "CLOSE_LIBRARY",
    "YIELD": "YIELD",
    "GET_TASK_ID": "GET_TASK_ID",
    "G_ALLOC_PEN": "GRAPHICS_ALLOC_PEN",
    "G_FREE_PEN": "GRAPHICS_FREE_PEN",
    "G_SET_PEN": "GRAPHICS_SET_PEN",
    "G_RECT": "GRAPHICS_RECT",
    "G_FILL_RECT": "GRAPHICS_FILL_RECT",
    "G_TEXT": "GRAPHICS_TEXT",
    "G_LINE": "GRAPHICS_LINE",
    "G_SET_CLIP": "GRAPHICS_SET_CLIP",
    "G_PEN_COLOR": "GRAPHICS_PEN_COLOR",
    "G_WRITE_PIXEL": "GRAPHICS_WRITE_PIXEL",
    "G_MOVE": "GRAPHICS_MOVE",
    "G_SET_PALETTE": "GRAPHICS_SET_PALETTE",
    "G_RASTER_INFO": "GRAPHICS_RASTER_INFO",
    "I_OPEN_SCREEN": "INTUITION_OPEN_SCREEN",
    "I_CLOSE_SCREEN": "INTUITION_CLOSE_SCREEN",
    "I_OPEN_WINDOW": "INTUITION_OPEN_WINDOW",
    "I_CLOSE_WINDOW": "INTUITION_CLOSE_WINDOW",
    "I_SET_TITLE": "INTUITION_SET_TITLE",
    "I_REFRESH": "INTUITION_REFRESH",
    "I_SET_ATTRS": "INTUITION_SET_ATTRS",
    "I_ADD_GADGET": "INTUITION_ADD_GADGET",
    "I_REMOVE_GADGET": "INTUITION_REMOVE_GADGET",
    "I_SET_BOUNDS": "INTUITION_SET_BOUNDS",
    "I_FRONT": "INTUITION_FRONT",
    "I_BACK": "INTUITION_BACK",
    "I_REPAINT": "INTUITION_REPAINT",
    "L_CREATE": "LAYERS_CREATE",
    "L_DESTROY": "LAYERS_DESTROY",
    "L_SET_CLIP": "LAYERS_SET_CLIP",
    "L_FRONT": "LAYERS_FRONT",
    "L_BACK": "LAYERS_BACK",
    "L_SET_VISIBLE": "LAYERS_SET_VISIBLE",
    "L_MOVE": "LAYERS_MOVE",
    "L_COMPOSITE": "LAYERS_COMPOSITE",
    "L_ORDER": "LAYERS_ORDER",
    "L_TOPMOST": "LAYERS_TOPMOST",
    "D_OPEN": "DOS_OPEN",
    "D_CLOSE": "DOS_CLOSE",
    "D_READ": "DOS_READ",
    "D_WRITE": "DOS_WRITE",
    "D_SEEK": "DOS_SEEK",
    "D_READ_DIR": "DOS_READ_DIR",
    "D_DELETE": "DOS_DELETE",
    "D_MKDIR": "DOS_MKDIR",
    "D_EXAMINE": "DOS_EXAMINE",
    "D_CURRENT_DIR": "DOS_CURRENT_DIR",
}

HEADER = """# The `{library}` IDL.
#
# One file per library, in the subset of YAML the SDK's build script parses.
# The script reads `library`, `prefix` and a `functions` list whose entries
# have `name`, `lvo`, `offset`, `stub`, `syscall`, `number`, `kernel` and
# `status` keys.
#
# The `lvo` values are the library vector offsets: negative, and one 8-byte
# word apart, so that `base + lvo` addresses the jump-table slot that holds
# the function. The build script *checks* that spacing and refuses to
# generate anything if it does not hold, which is what makes the table
# coherent by construction rather than by review.
#
# The `offset` value is the AmigaOS spelling of the function's name, without
# underscores — `PUTMSG`, `OPENWINDOW` — because that is the name a ported
# program is already using. The generated constant is
# `<PREFIX>_<OFFSET>_OFFSET`.
#
# The `stub` value is the generated stub function's name. It is spelled out
# rather than derived because the stub names are the SDK's public API, and a
# mechanical snake-case rule would have renamed `CreateMsgPort` to
# `create_msg_port` and `FindTask` to `find_task` — neither of which is what
# the SDK calls them.
#
# The `number` values are the kernel's syscall numbers. The build script
# checks each one against the SDK's own `numbers.rs`, and the SDK's hosted
# test checks each one against the kernel's `numbers` module — so a drift
# between this file and the kernel is a build-time or test-time failure
# rather than a wrong call at runtime.
#
# `syscall: none` marks a function the kernel does not expose. Its stub
# reports `NOT_IMPLEMENTED` rather than issuing some other library's call,
# because a stub that quietly did the wrong thing would be worse than one
# that says it does nothing.

library: {library}
prefix: {prefix}
functions:
"""

# The functions the kernel does not implement, by AmigaOS name. The status
# column of the generated conformance document says so.
NOT_STARTED = {"MapPage", "Brk", "AddPort"}


def main():
    for module, library, prefix, functions in LIBRARIES:
        lines = [HEADER.format(library=library, prefix=prefix)]
        for (name, lvo, suffix, stub, syscall, number) in functions:
            status = "not-started" if name in NOT_STARTED else "implemented"
            lines.append("  - name: %s\n" % name)
            lines.append("    lvo: %d\n" % lvo)
            lines.append("    offset: %s\n" % suffix)
            lines.append("    stub: %s\n" % stub)
            if syscall is None:
                lines.append("    syscall: none\n")
            else:
                lines.append("    syscall: %s\n" % syscall)
                lines.append("    number: %d\n" % number)
                lines.append("    kernel: %s\n" % KERNEL_NAMES[syscall])
            lines.append("    status: %s\n" % status)
        path = "sdk/vela-sys/idl/%s.yaml" % module
        with open(path, "w") as f:
            f.write("".join(lines))
        print("wrote", path)


if __name__ == "__main__":
    main()
