// rewrite-it KWin script
//
// Registers a global shortcut at the KWin compositor level so it is always
// grabbed — regardless of which application is focused — and calls the
// rewrite-it DBus service to rewrite the current clipboard/selection.
//
// The shortcut is grabbed by KWin itself, so it works on both X11 and Wayland,
// and does not depend on kglobalaccel component registration or KHotKeys
// (which was removed in Plasma 6).

registerShortcut(
    "rewrite-grammar",               // unique action id
    "Help me rewrite (grammar)",     // human label shown in System Settings
    "Meta+Shift+G",                  // default key (free on stock Plasma 6)
    function () {
        // Ask the rewrite-it daemon to read the clipboard/selection, rewrite
        // it with grammar correction, and copy the result back.
        // If the daemon is not running, DBus activation starts it automatically.
        //
        // KWin's documented scripting API exposes callDBus(), but not direct
        // key synthesis. The daemon therefore handles the follow-up paste
        // attempt on Wayland via the XDG Remote Desktop portal and otherwise
        // falls back to leaving the rewritten text on the clipboard.
        callDBus(
            "org.rewriteit.Rewriter1",   // service name
            "/org/rewriteit/Rewriter",   // object path
            "org.rewriteit.Rewriter1",   // interface
            "RewriteSelection",          // method
            "grammar",                   // style argument
            function () {}
        );
    }
);
