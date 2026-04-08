// rewrite-it KWin script
//
// Registers global shortcuts at the KWin compositor level so they are always
// grabbed — regardless of which application is focused — and calls the
// rewrite-it DBus service to rewrite the current clipboard/selection.
//
// Shortcuts are grabbed by KWin itself, so they work on both X11 and Wayland,
// and do not depend on kglobalaccel component registration or KHotKeys
// (which was removed in Plasma 6).
//
// Default bindings (all free on stock Plasma 6):
//   Meta+Shift+G  →  grammar
//   Meta+Shift+F  →  formal
//   Meta+Shift+C  →  concise
//
// Users can rebind or add shortcuts for casual/elaborate/creative in
// System Settings → Keyboard → Shortcuts → KWin → rewrite-it.

function rewriteSelection(style) {
    callDBus(
        "org.rewriteit.Rewriter1",
        "/org/rewriteit/Rewriter",
        "org.rewriteit.Rewriter1",
        "RewriteSelection",
        style,
        function () {}
    );
}

registerShortcut(
    "rewrite-grammar",
    "Rewrite selection (grammar)",
    "Meta+Shift+G",
    function () { rewriteSelection("grammar"); }
);

registerShortcut(
    "rewrite-formal",
    "Rewrite selection (formal)",
    "Meta+Shift+F",
    function () { rewriteSelection("formal"); }
);

registerShortcut(
    "rewrite-concise",
    "Rewrite selection (concise)",
    "Meta+Shift+C",
    function () { rewriteSelection("concise"); }
);

registerShortcut(
    "rewrite-casual",
    "Rewrite selection (casual)",
    "",
    function () { rewriteSelection("casual"); }
);

registerShortcut(
    "rewrite-elaborate",
    "Rewrite selection (elaborate)",
    "",
    function () { rewriteSelection("elaborate"); }
);

registerShortcut(
    "rewrite-creative",
    "Rewrite selection (creative)",
    "",
    function () { rewriteSelection("creative"); }
);
