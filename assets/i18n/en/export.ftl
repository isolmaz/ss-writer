# Export, print and share (ssw-app ui::export): file dialogs, results and failures.

export-save-title = Export
export-filter-all = All files
export-filter-textpack = TextBundle package
export-filter-textbundle = TextBundle folder
export-diagram-png = PNG image
export-diagram-svg = SVG image

export-done-title = Export finished
export-done-one = { $name } was exported to { $path }.
export-done-many =
    { $count ->
        [one] 1 page
       *[other] { $count } pages
    } of { $name } were exported to { $path }.
export-done-question = Open it now? Choose Yes to open the file, No to show it in its folder.

export-failed-title = Export failed
export-failed-message = { $name } could not be exported as { $format }. { $reason }
export-failed-write = The file { $path } could not be written: { $reason }

export-nothing-title = Nothing to export
export-nothing-message = Open a document first.

export-reason-empty = The export produced no pages.
export-reason-busy = The export could not be started. { $reason }
export-reason-crashed = The exporter stopped unexpectedly.
export-reason-same-file = Choose a different file: exporting over the document itself would replace it.

export-mermaid-failed = The diagram could not be drawn, so there is nothing to export.
export-mermaid-title = Diagram

export-print-title = Print
export-print-failed = { $name } could not be printed. { $reason }
export-print-unavailable = Printing is not available in this window.

export-share-title = Share
export-share-failed = { $name } could not be shared. { $reason }
export-share-unsaved = Save the document first: only files can be shared.
export-share-unavailable = Sharing is not available in this window.
