# Application core messages (ssw-app): startup, document names, file errors, import report.

app-os-too-old-title = SS Writer
app-os-too-old-message = SS Writer needs Windows 11 version 22H2 (build 22621) or later. This computer runs build { $build }.
app-startup-failed-title = SS Writer cannot start
app-startup-failed-message = SS Writer could not start: { $reason }

app-dialog-save = Save
app-dialog-dont-save = Don't save

app-untitled = Untitled
app-untitled-numbered = Untitled { $n }

app-error-not-found = The file no longer exists.
app-error-encrypted = The document is password protected and cannot be opened.
app-error-conversion = The document could not be converted.
app-error-file = The file could not be read or written.
app-error-temp-kept = The file could not be replaced. Its new content is kept in { $path }.
app-error-not-text = This file is not a text file.
app-error-read-only = This file is read-only.
app-error-in-use = The file is in use by another program.
app-error-disk-full = The disk is full.
app-error-access-denied = Access to the file was denied.
app-error-encoding = Some characters cannot be saved in this encoding.
app-error-untitled = The document has no file yet.
app-error-needs-save-as = Imported documents are saved with Save as, or saved back to their original format.
app-error-unsupported = This format cannot be written.
app-error-already-open = This file is already open in another tab.

# One box for the files that a single action could not open: a heading, then a line per file.
app-open-failed = { $count ->
    [one] The file could not be opened:
   *[other] { $count } files could not be opened:
}
app-open-failed-file = { $path }: { $reason }

app-loss-merged-cells = Merged table cells
app-loss-headers-footers = Headers and footers
app-loss-columns = Text columns
app-loss-text-boxes = Text boxes
app-loss-comments = Comments
app-loss-tracked-changes = Tracked changes
app-loss-vector-shapes = Drawing shapes
app-loss-unsupported-equations = Equations that could not be converted
app-loss-unsupported-images = Images in unsupported formats
app-loss-alignment = Paragraph alignment
app-loss-underline = Underline styles
app-loss-fonts-and-colors = Fonts and colors
app-loss-sub-superscript = Subscript and superscript
app-loss-nested-tables = Nested tables
app-loss-multi-paragraph-cells = Several paragraphs in one table cell
app-loss-fields = Fields
app-loss-hidden-text = Hidden text
app-loss-symbol-font-characters = Symbol font characters
app-loss-list-numbering-style = List numbering styles
app-loss-section-breaks = Section breaks
app-loss-corrupt-parts = Damaged parts of the file
