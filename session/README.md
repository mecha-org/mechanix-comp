# Session resources

| File | Install to |
| --- | --- |
| `mechanix-session` | `/usr/bin/mechanix-session` (0755) |
| `mechanix.desktop` | `/usr/share/wayland-sessions/` |
| `mechanix-compositor.service` | `/usr/lib/systemd/user/` |
| `etnaviv.conf` | `/usr/lib/systemd/user/mechanix-compositor.service.d/` |
| `mechanix-session.target` | `/usr/lib/systemd/user/` |
| `mechanix-session-shutdown.target` | `/usr/lib/systemd/user/` |
| `mechanix-shell.service` | `/usr/lib/systemd/user/` |
| `mechanix-keyboard.service` | `/usr/lib/systemd/user/` |
| `mechanix-waybar.service` | `/usr/lib/systemd/user/` |
| `mechanix-drawer.service` | `/usr/lib/systemd/user/` |
| `mechanix-portals.conf` | `/usr/share/xdg-desktop-portal/` |
| `xdg-desktop-portal-wlr.conf` | `/etc/xdg/xdg-desktop-portal-wlr/mechanix` |

`mechanix-waybar.service` and `mechanix-drawer.service` are temporary.

## Changing the shell

The session runs whatever `mechanix-session.target` wants. To leave a part out,
mask it:

```sh
systemctl --user mask mechanix-waybar.service
```

To add one, put a unit like this in `~/.config/systemd/user/` and enable it with
`systemctl --user enable my-panel.service`:

```ini
[Unit]
BindsTo=mechanix-compositor.service
After=mechanix-compositor.service
PartOf=graphical-session.target

[Service]
ExecStart=/usr/bin/my-panel
Slice=session.slice

[Install]
WantedBy=mechanix-session.target
```
