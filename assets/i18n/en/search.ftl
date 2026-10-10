# Search, navigation overlays and editor popups (ssw-ui shell: find, goto, quick_open, folder_search, popups).

## Find bar

search-find-bar = Find and replace
search-find-label = Find
search-find-placeholder = Find
search-find-match-case = Match case
search-find-match-case-glyph = Aa
search-find-whole-word = Whole word
search-find-whole-word-glyph = ab
search-find-regex = Regular expression
search-find-regex-glyph = .*
search-find-previous = Previous match
search-find-next = Next match
search-find-close = Close
search-find-toggle-replace = Toggle replace
search-find-counter = { $current }/{ $total }
search-find-no-results = No results
search-find-invalid-regex = Invalid regular expression
search-replace-label = Replace with
search-replace-placeholder = Replace
search-replace-one = Replace
search-replace-all = Replace all

## Go to line

search-goto-label = Go to line
search-goto-placeholder = Line number, 1 to { $count }
search-goto-hint = Type a line number between 1 and { $count }
search-goto-invalid = Not a valid line number

## Quick Open and command palette

search-quick-open-label = Quick Open
search-quick-open-placeholder = Search files by name. Type # for headings, > for commands
search-quick-open-results = Results
search-quick-open-no-files = No matching files
search-quick-open-no-headings = No matching headings
search-quick-open-no-commands = No matching commands
search-quick-open-heading-level = H{ $level }

## Search in folder

search-folder-title = Search in folder
search-folder-placeholder = Search
search-folder-query-label = Search in folder
search-folder-results = Search results
search-folder-searching = Searching…
search-folder-no-results = No results found
search-folder-matches = { $matches ->
    [one] 1 result
   *[other] { $matches } results
}
search-folder-files = { $files ->
    [one] 1 file
   *[other] { $files } files
}
search-folder-summary = { $matches } in { $files }
search-folder-truncated = Only the first { $matches } are shown
search-folder-invalid-regex = Invalid regular expression
search-folder-file-header = { $path }, { $matches }
search-folder-line-name = { $path }, line { $line }: { $preview }

## Link popup

search-link-title = Edit link
search-link-text = Text
search-link-url = Address
search-link-apply = Apply
search-link-remove = Remove link

## Wikilink completion

search-wikilink-label = Link suggestions

## Heading level menu

search-heading-menu = Heading level

## Selection toolbar

search-toolbar-label = Formatting
search-toolbar-heading = Heading
search-toolbar-task = Task
search-toolbar-list = List
search-toolbar-highlight = Highlight color
search-toolbar-more = More formatting
search-color-red = Red
search-color-orange = Orange
search-color-yellow = Yellow
search-color-green = Green
search-color-blue = Blue
search-color-purple = Purple

## B I U pill

search-pill-label = Text style
search-pill-bold-glyph = B
search-pill-italic-glyph = I
search-pill-underline-glyph = U
