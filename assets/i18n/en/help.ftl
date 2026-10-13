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

# Downloading and installing the new version.
help-update-downloading-title = Downloading SS Writer { $version }
help-update-downloading-message = { $done } of { $total } MB downloaded.
help-update-downloading-unknown = { $done } MB downloaded.
help-update-ready-title = SS Writer { $version } is ready to install
help-update-ready-message = SS Writer closes, installs the update and opens again.
help-update-restart = Restart and install
help-update-download-failed-title = Download failed
help-update-download-failed-message = SS Writer could not download the update. { $detail }
help-update-install-failed-message = SS Writer could not start the installer. { $detail }

# "Check for updates" results.
help-update-current-title = Check for updates
help-update-current-message = SS Writer { $version } is the latest version.
help-update-failed-title = Check for updates
help-update-failed-message = SS Writer could not check for updates. { $detail }
