# Rinputer3, this time in Rust

Maps *every* gamepad under the sun into Xbox 360 gamepad, maintaining 1:1 compatibility.
Requires read access to /dev/input/event\* and write access to /dev/uinput
With fully dynamic mapping of *any* button or axis to *any* button or axis

For IPC open socket `/var/run/rinputer.sock` and add `-i` flag
It's also planned to have a `talk2rinputer`-ish program that would simplify this
IPC Commands:
- `reset` - Resets config to default
- `print` - Prints config
- `rescan`(TODO) - Rescans devices
- `map <code> as <code>` maps digital button to other digital button
- `map <axis>@<level> as <code>` maps axis being further away than `<level>` as `<code>`
- `map <axis>@<level> as <axis>@<level>` maps axis between 0 and `<level>` as other axis between 0 and `<level>`. Does multiplication magic to remap between any values. You can map axes with different min/max levels
- `map <code> as <axis>@<level>` maps pressing `<code>` as `<axis>` reaching `<level>`, depressing `<code>` will be zeroing out `<axis>`

NOTE: there is a special event code, `SteamQuickAccess` that will do a `BTN_MODE`+`BTN_SOUTH` combination to launch Steam gamepadui quick access menu.
