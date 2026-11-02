# Startup improvements

Approved improvements implemented:

- [x] Top-right, keyboard-accessible information icons for each permission box.
- [x] **Turn on internet for the container** below the URL field, on for fresh states, always subject to the whitelist. Existing on/off choices are preserved.
- [x] Offline website-name suggestions with canonical HTTPS URLs.
- [x] Removable read/write default grant for `/tmp/omp-manager`, used through the revocable folder gateway inside the container.
- [x] Automatic AMD/NVIDIA detection with manual override (NVIDIA hardware not available for verification).
- [x] Common-folder dropdown respecting localized XDG folders.
- [x] Installed Nix application-name search with local dropdown suggestions.
- [x] Visible, removable OMBP runtime bundles and private sandbox mounts; disabled choices persist. Removing requirements can break startup. Private mount changes need a restart; store-root and host-folder revocations apply live.
- [x] Four removable default websites: GitHub, Google, Stack Overflow, Atlassian. Defaults are seeded once and do not reappear after removal.
- [x] Denied websites grouped under their requesting site with per-endpoint checkboxes and group +/−; per-site opt-in HTML/CSS (served-content) and Header (forgeable) dependency modes.
- [x] Per-site Subdomains checkbox (default on, leading www. ignored); Auto-approve toggle with editable list (anthropic.com, chatgpt.com, google.com); approved Denied rows are removed from history.
- [x] Default removable Basic tools bundle (gdbus, procps, psmisc, util-linux, grep/sed/awk/find, archives, curl/wget/jq/rg/fd, rsync, make, strace, lsof…); Allow all network traffic switch at the top of Denied attempts.
- [x] Clear denied-attempt history without granting access; subsequent attempts can appear again.

Existing fixes retained: build the selected OMBP source checkout on each Start click, cancel pending builds on launcher shutdown, remove only the inner Weston frame, and tolerate bounded input bursts while OMBP is busy.
