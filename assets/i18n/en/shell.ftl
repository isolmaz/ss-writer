# SS Writer — English. Window shell: title row, tabs, sidebar, inspector, status bar, banners.

## Title row and command row

titlebar-title-bar = Title bar
titlebar-tabs = Tabs
titlebar-commands = Commands
titlebar-format-pill = Text formatting
# The letters shown on the formatting pill buttons.
titlebar-bold-face = B
titlebar-italic-face = I
titlebar-underline-face = U

## Tabs

tabs-label = Open documents
tabs-new = New tab
tabs-unsaved = Unsaved changes
tabs-untitled = Untitled

## Status bar

statusbar-label = Status bar
statusbar-position = Ln { $line }, Col { $column }
statusbar-words =
    { $count ->
        [one] { $count } word
       *[other] { $count } words
    }
statusbar-words-selected =
    { $selected } of { $count ->
        [one] { $count } word
       *[other] { $count } words
    }
statusbar-zoom = { $percent }%
statusbar-eol-lf = LF
statusbar-eol-crlf = CRLF
statusbar-eol-cr = CR
statusbar-encoding-hint = Change encoding
statusbar-eol-hint = Change line endings

## Banners

banner-label = Notifications
banner-close = Close
banner-details = Details
banner-details-list = Details
banner-details-more = … and { $count } more
banner-read-only-title = This file is read-only
banner-read-only-message = Allow editing to change it.
banner-allow-editing = Allow editing
banner-conflict-title = The file changed on disk
banner-conflict-message = Your unsaved changes are not in the new version.
banner-keep-mine = Keep mine
banner-load-theirs = Load theirs
banner-compare = Compare
banner-import-title = Imported with changes
banner-import-message =
    { $count ->
        [one] { $count } thing could not be represented
       *[other] { $count } things could not be represented
    }
banner-remote-images-title = Remote images are loaded
banner-remote-images-message = Pictures from the web are shown in this document.
banner-dont-load-remote-images = Don't load remote images
banner-recovery-title = Recovered unsaved work
banner-recovery-message = SS Writer closed unexpectedly. Restore the recovered text?
banner-restore = Restore
banner-discard = Discard

## Inspector

inspector-label = Inspector
inspector-contents = Contents
inspector-info = Info
inspector-level-filter = Heading levels
inspector-level = H{ $level }
inspector-no-headings = No headings
inspector-document = Document
inspector-selection = Selection
inspector-words = Words
inspector-characters = Characters
inspector-characters-no-spaces = Characters (no spaces)
inspector-paragraphs = Paragraphs
inspector-sentences = Sentences
inspector-reading-time = Reading time
inspector-minutes = { $minutes } min
inspector-file = File
inspector-no-file = No file
inspector-path = Path
inspector-size = Size
inspector-created = Created
inspector-modified = Modified
inspector-encoding = Encoding
inspector-line-endings = Line endings
inspector-backlinks = Backlinks ({ $count })
inspector-no-backlinks = No backlinks

## Sidebar

sidebar-tree = Files
sidebar-more = More options
sidebar-new = New
sidebar-rename = Rename
sidebar-cloud = Available online only
sidebar-empty = This folder is empty
