# SS Writer — workspace file flows (ssw-app ui/files.rs): link updates after a rename or move,
# wikilinks to notes that do not exist yet.

# After a rename or move: some other documents link to the file.
files-links-title = Update links
files-links-message = { $files ->
    [one] The links in 1 other file point to { $name }. Update them to its new location?
   *[other] The links in { $files } other files point to { $name }. Update them to its new location?
}

# Some of those documents could not be changed (unreadable, changed meanwhile, read-only).
files-links-failed = { $files ->
    [one] 1 file could not be updated. Its links still point to the old location.
   *[other] { $files } files could not be updated. Their links still point to the old location.
}

# Ctrl+click on a wikilink whose note does not exist.
files-wikilink-title = Create note
files-wikilink-create = There is no note called { $name } in this workspace. Create it?
files-wikilink-invalid = A note called { $name } cannot be created inside this workspace.
