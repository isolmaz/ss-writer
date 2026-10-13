# Settings window, version history panel and export dialog (ssw-ui shell).
# Page titles come from main.ftl (settings-section-*), command names from main.ftl (cmd-*).

# --- Settings window -------------------------------------------------------------------------
settings-title = Settings
settings-nav = Settings pages
settings-scroll = Page scroll bar
settings-browse = Browse…
settings-percent = { $value }%

settings-hint-dips = Size in DIPs (1/96 inch).
settings-hint-percent = Percent.
settings-hint-wpm = Words per minute.
settings-hint-mm = Millimeters.
settings-hint-pt = Points.
settings-hint-days = Days.

# --- General -----------------------------------------------------------------------------------
settings-general-language = Language
settings-opt-language-system = System default
settings-general-startup = On startup
settings-opt-startup-restore = Restore the previous session
settings-opt-startup-new = Open a new document
settings-opt-startup-folder = Open the default folder
settings-general-default-folder = Default folder
settings-general-autosave = Autosave
settings-general-autosave-delay = Autosave delay
settings-general-autosave-delay-hint = Milliseconds after you stop typing (100 to 60000).
settings-general-new-file-location = New file location
settings-opt-new-file-selected = Selected folder
settings-opt-new-file-default = Default folder
settings-general-sync-file-name = Sync file name with first heading
settings-general-check-updates = Check for updates
settings-general-default-apps = Default app
settings-general-default-apps-hint = Opens Windows Settings, where you choose the app that opens Markdown and text files.
settings-general-default-apps-button = Default apps…

# --- Appearance --------------------------------------------------------------------------------
settings-appearance-theme = Theme
settings-opt-theme-system = Use system setting
settings-opt-theme-light = Light
settings-opt-theme-dark = Dark
settings-appearance-accent = Accent color
settings-opt-accent-system = Windows accent color
settings-opt-accent-blue = Blue
settings-opt-accent-custom = Custom
settings-appearance-accent-custom = Custom accent color
settings-appearance-accent-custom-hint = Used when the accent is Custom. Write it as #RRGGBB.
settings-appearance-mica = Mica backdrop
settings-appearance-sidebar-font-size = Sidebar font size
settings-appearance-status-bar = Show status bar
settings-appearance-format-pill = Show the B I U formatting pill
settings-appearance-zoom = Zoom

# --- Editor ------------------------------------------------------------------------------------
settings-editor-text-font = Text font
settings-editor-heading-font = Heading font
settings-editor-code-font = Code font
settings-editor-font-size = Font size
settings-editor-line-height = Line height
settings-editor-line-height-hint = A multiple of the font size (1 to 3).
settings-editor-paragraph-spacing = Paragraph spacing
settings-editor-paragraph-spacing-hint = Extra space between paragraphs, in lines (0 to 3).
settings-editor-line-width = Line width
settings-editor-list-indent = List indent
settings-opt-indent-align = Align with the item text
settings-opt-indent-2 = 2 spaces
settings-opt-indent-4 = 4 spaces
settings-opt-indent-tab = Tab
settings-editor-format-indent = Format indent
settings-editor-auto-hide = Hide Markdown markers away from the caret
settings-editor-typewriter-position = Typewriter position
settings-editor-typewriter-dead-zone = Typewriter dead zone
settings-editor-spell-check = Check spelling
settings-editor-spell-languages = Spell-check languages
settings-editor-spell-languages-hint = Language tags separated by commas, such as en-US, tr. Empty uses the Windows language.
settings-editor-sort-tasks = Move completed tasks to the end of their list
settings-editor-syntax-highlighting = Highlight code in code blocks
settings-editor-format-tables = Format tables when the caret leaves them
settings-editor-hide-front-matter = Hide the YAML front matter card
settings-editor-reading-speed = Reading speed

# --- Sidebar -----------------------------------------------------------------------------------
settings-sidebar-sort = Sort by
settings-opt-sort-name = Name
settings-opt-sort-modified = Date modified
settings-opt-sort-created = Date created
settings-opt-sort-kind = Kind
settings-sidebar-descending = Descending order
settings-sidebar-folders-first = Folders first
settings-sidebar-extensions = Show file extensions
settings-sidebar-hidden = Show hidden items
settings-sidebar-hide-images = Hide images
settings-sidebar-hide-non-markdown = Hide files that are not Markdown
settings-sidebar-modified-date = Show the modified date under names

# --- Attachments -------------------------------------------------------------------------------
settings-attachments-location = Pasted images are saved
settings-opt-attach-next = Next to the file
settings-opt-attach-assets = In an assets folder next to the file
settings-opt-attach-file-assets = In a folder named after the file
settings-opt-attach-custom = In a custom folder
settings-attachments-custom-folder = Custom folder
settings-attachments-template = File name template
settings-attachments-template-hint = { "{n}" } is replaced by a running number.
settings-attachments-remote-images = Load remote images

# --- Export ------------------------------------------------------------------------------------
settings-export-page-size = Page size
settings-opt-page-a4 = A4
settings-opt-page-letter = Letter
settings-export-margin = Margins
settings-export-font-size = Base font size
settings-export-page-numbers = Page numbers
settings-export-black-text = Black text
settings-export-light-theme = Light theme
settings-export-include-title = Include the title
settings-export-default-format = Default format
settings-opt-format-pdf = PDF
settings-opt-format-png = PNG image
settings-opt-format-jpg = JPG image
settings-opt-format-html = HTML (single file)
settings-opt-format-epub = ePub
settings-opt-format-docx = Word (DOCX)
settings-opt-format-odt = OpenDocument (ODT)
settings-opt-format-rtf = Rich Text (RTF)
settings-opt-format-txt = Plain text
settings-opt-format-markdown = Markdown with attachments
settings-opt-format-textbundle = TextBundle

# --- Advanced ----------------------------------------------------------------------------------
settings-advanced-index-max-files = Largest folder index
settings-advanced-index-max-files-hint = Most files the workspace index keeps (1000 to 10000000).
settings-advanced-history-retention = Keep version history for
settings-advanced-open-logs = Logs
settings-advanced-open-logs-button = Open logs folder
settings-advanced-open-file = Settings file
settings-advanced-open-file-hint = Edit settings.json by hand; changes you save apply right away.
settings-advanced-open-file-button = Open settings.json
settings-advanced-reset = Reset settings
settings-advanced-reset-hint = Puts every setting back to its default, shortcuts included.
settings-advanced-reset-button = Reset all settings

# --- Validation messages -------------------------------------------------------------------------
settings-error-number = Enter a number.
settings-error-integer = Enter a whole number.
settings-error-range = Enter a value from { $min } to { $max }.
settings-error-empty = This cannot be empty.
settings-error-color = Write the color as #RRGGBB.
settings-error-template = The name must contain { "{n}" } and none of \ / : * ? " < > |
settings-error-path = Windows does not allow < > " | ? * in a path.
settings-error-language = { $tag } is not a language tag. Examples: en-US, tr.

# --- Shortcuts ---------------------------------------------------------------------------------
settings-shortcut-filter = Filter commands
settings-shortcut-list = Commands and their shortcuts
settings-shortcut-row = { $command } — { $chords }
settings-shortcut-row-conflict = { $command } — { $chords } — conflicts with { $others }
settings-shortcut-none = Not assigned
settings-shortcut-change = Change
settings-shortcut-add = Add another
settings-shortcut-clear = Remove
settings-shortcut-reset = Reset
settings-shortcut-reset-all = Reset all
settings-shortcut-capture = Press the new shortcut for { $command }. Esc cancels.
settings-shortcut-assigned = { $command }: { $chords }
settings-shortcut-conflict = { $chord } is also used by { $others }. Neither command runs until one of them is changed.
settings-shortcut-altgr = { $chord } types a character on your keyboard layout (AltGr), so it cannot be a shortcut. Press another one.

# --- Version history -----------------------------------------------------------------------------
history-title = Version history
history-list = Saved versions
history-diff = Changes since this version
history-empty = There are no saved versions of this document yet.
history-select = Select a version to see what changed.
history-loading = Comparing…
history-identical = This version is identical to the current document.
history-entry = { $time } — { $size }
history-restore = Restore this version
history-close = Close
history-size-bytes = { $value } bytes
history-size-kb = { $value } KB
history-size-mb = { $value } MB
history-hunk = Lines { $oldStart }–{ $oldEnd } before, { $newStart }–{ $newEnd } now
history-line-added = Added: { $text }
history-line-removed = Removed: { $text }
history-line-context = { $text }

# --- Export dialog -----------------------------------------------------------------------------
export-dialog-title = Export
export-dialog-formats = Format
export-dialog-options = Options
export-dialog-scroll = Options scroll bar
export-dialog-export = Export
export-dialog-cancel = Cancel
