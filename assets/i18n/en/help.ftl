# SS Writer — Help menu: documents, About box, update check (ssw-app ui/help.rs).

# Alt+letter that opens the Help menu.
help-menu-key = H

# File names (without extension) of the help documents; they are the tab titles, so they must
# be valid Windows file names.
help-welcome-file = Welcome
help-shortcuts-file = Keyboard shortcuts
help-notices-file = Third-party notices

# Keyboard shortcut document.
help-shortcuts-title = Keyboard shortcuts
help-shortcuts-intro = Every shortcut can be changed in Settings.
help-shortcuts-command = Command
help-shortcuts-keys = Shortcut

# About box (Yes opens the third-party notices).
help-about-title = About SS Writer
help-about-message =
    SS Writer { $version }
    Markdown & Text Editor

    Licensed under the MIT License.
    Copyright © 2026 isolmaz.
    https://github.com/isolmaz/ss-writer

    SS Writer includes open-source components. Their names, versions and licenses are listed in the third-party notices.

    Do you want to open the third-party notices now?

# Update notification banner.
help-update-title = A new version of SS Writer is available
help-update-message = SS Writer { $version } is available. You are using { $current }.
help-update-download = Download

# "Check for updates" results.
help-update-current-title = Check for updates
help-update-current-message = SS Writer { $version } is the latest version.
help-update-failed-title = Check for updates
help-update-failed-message = SS Writer could not check for updates. { $detail }
