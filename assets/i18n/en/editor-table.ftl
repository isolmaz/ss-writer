# SS Writer — table renderer of the editor (English, source locale).

# Accessible description of a table drawn as a grid (UI Automation). $rows counts the header row.
table-access =
    Table with { $rows ->
        [one] 1 row
       *[other] { $rows } rows
    } and { $columns ->
        [one] 1 column
       *[other] { $columns } columns
    }

# Accessible description of a table shown as raw Markdown.
table-access-raw = Table, raw Markdown
