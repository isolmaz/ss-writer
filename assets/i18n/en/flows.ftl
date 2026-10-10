# SS Writer — version history panel, banner flows and the recovery offer (ssw-app ui).

# Version history: the time of a saved version in the list. { $date } is the short date of the
# user's locale, { $hour }:{ $minute } the time of day in UTC (two digits each).
flows-history-time = { $date } { $hour }:{ $minute } UTC

# Version history: the entry that stands for the file as it is on disk right now (opened from
# the "Compare" button of the conflict banner).
flows-history-on-disk = File on disk

# Version history: a saved version could not be read.
flows-history-missing = That saved version is no longer available.

# Recovery offer: one document of the crashed session could not be brought back.
flows-recovery-failed = The recovered text of { $name } could not be restored: { $reason }

# Recovery offer: the recovered text could not be deleted (Discard failed).
flows-recovery-discard-failed = The recovered text could not be deleted: { $reason }

# Recovery offer: closing the banner would delete the recovered text.
flows-recovery-discard-title = Discard recovered work
flows-recovery-discard-message = Delete the recovered unsaved text? It cannot be restored afterwards.
