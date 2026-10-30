Recall for Omarchy v0.6.0 installs from a signed pacman repository and updates with the rest of the system.

- Install or move to the package with `curl -fsSL https://ferdousbhai.com/recall/install.sh | sudo bash`. It adds the [recall-for-omarchy] repository (signed with the same key as Ferdous Bhai's other Omarchy apps), installs `recall-for-omarchy` with its dependencies (and `vulkan-asahi` on Apple hardware), then runs `recall setup` for you in your desktop session. `RECALL_NO_KEYBINDING=1` still skips the keybinding: `| sudo RECALL_NO_KEYBINDING=1 bash`.
- Updates arrive through `omarchy update`. An upgrade restarts the running service, which then refreshes your panel and agent skill.
- Moving from an earlier tarball install keeps your settings, index and models; setup points the launchers and the service at the package and removes the old copies under `~/.local/share/recall-for-omarchy/releases`.
- Packages are built natively for x86_64 and aarch64 from the tagged source, with only relative library search paths.

Based on pdstn12/Recall (MIT), inspired by ARahim3/DigUp.
