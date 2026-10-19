# Protocol Support

✅ implemented · 🟡 loose · 🚧 partial (handler only, no global) · ❌ missing.

Version is `ours / spec`.

Smithay: ❌ means we hand-roll it.

## Core

| Protocol | | Version | Smithay | Notes |
| --- | --- | --- | --- | --- |
| `wl_compositor` | ✅ | 5 / 6 | ✅ | |
| `wl_subcompositor` | ✅ | 1 / 1 | ✅ | |
| `wl_shm` | ✅ | 2 / 2 | ✅ | |
| `wl_seat` | ✅ | 9 / 9 | ✅ | |
| `wl_output` | ✅ | 4 / 4 | ✅ | |
| `wl_data_device_manager` | ✅ | 3 / 3 | ✅ | |

## Stable

| Protocol | | Version | Smithay | Notes |
| --- | --- | --- | --- | --- |
| `xdg-shell` | ✅ | 7 / 7 | ✅ | |
| `zwp_linux_dmabuf_v1` | ✅ | 6 / 6 | ✅ | v3 fallback without a render node |
| `wp_presentation` | ✅ | 2 / 2 | ✅ | |
| `wp_viewporter` | ✅ | 1 / 1 | ✅ | |
| `wp_single_pixel_buffer_manager_v1` | ✅ | 1 / 1 | ✅ | |

## Staging

| Protocol | | Version | Smithay | Notes |
| --- | --- | --- | --- | --- |
| `wp_commit_timing_manager_v1` | ✅ | 1 / 1 | ✅ | |
| `wp_cursor_shape_manager_v1` | ✅ | 2 / 2 | ✅ | |
| `wp_fifo_manager_v1` | ✅ | 1 / 1 | ✅ | |
| `wp_fractional_scale_manager_v1` | ✅ | 1 / 1 | ✅ | |
| `wp_linux_drm_syncobj_manager_v1` | ✅ | 1 / 1 | ✅ | |
| `ext_foreign_toplevel_list_v1` | ✅ | 1 / 1 | ✅ | |
| `ext_idle_notifier_v1` | ✅ | 2 / 2 | ✅ | |
| `ext_image_capture_source_v1` | ✅ | 1 / 1 | ✅ | |
| `ext_image_copy_capture_v1` | 🟡 | 1 / 1 | ✅ | recorders can't capture the cursor separately |
| `ext_session_lock_manager_v1` | ✅ | 1 / 1 | ✅ | |
| `xdg_activation_v1` | 🟡 | 1 / 1 | ✅ | ignores `app_id`/`serial` |
| `xdg_wm_dialog_v1` | ✅ | 1 / 1 | ✅ | |
| `xdg_toplevel_icon_manager_v1` | ✅ | 1 / 1 | ✅ | |

## Unstable

| Protocol | | Version | Smithay | Notes |
| --- | --- | --- | --- | --- |
| `zwp_primary_selection_device_manager_v1` | ✅ | 1 / 1 | ✅ | |
| `zwp_text_input_manager_v3` | ✅ | 1 / 1 | ✅ | |
| `zwp_input_method_manager_v2` | ✅ | 1 / 1 | ✅ | |
| `zwp_virtual_keyboard_manager_v1` | ✅ | 1 / 1 | ✅ | |
| `zwp_idle_inhibit_manager_v1` | ✅ | 1 / 1 | ✅ | |
| `zxdg_decoration_manager_v1` | ✅ | 1 / 1 | ✅ | server-side but no titlebars drawn (intentionally) |
| `zxdg_output_manager_v1` | ✅ | 3 / 3 | ✅ | |
| `zxdg_exporter_v2` / `zxdg_importer_v2` | ✅ | 1 / 1 | ✅ | |
| `zwp_pointer_constraints_v1` | 🚧 | — / 1 | ✅ | no global |
| `zwp_relative_pointer_v1` | ❌ | — / 1 | ✅ | |
| `zwp_tablet_v2` | 🚧 | — / 1 | ✅ | no global |

## Unstandardised

| Protocol | | Version | Smithay | Notes |
| --- | --- | --- | --- | --- |
| `zwlr_layer_shell_v1` | ✅ | 5 / 5 | ✅ | |
| `zwlr_data_control_manager_v1` | ✅ | 2 / 2 | ✅ | |
| `zwlr_foreign_toplevel_manager_v1` | ✅ | 3 / 3 | ❌ | `activate`/`close` works, state requests ignored |
| `zwlr_output_power_manager_v1` | ✅ | 1 / 1 | ❌ | |

## Missing

| Protocol | Smithay |
| --- | --- |
| `xwayland_shell_v1` | ✅ |
| `wl_drm` | ❌ |
| `wp_keyboard_shortcuts_inhibit_v1` | ✅ |
| `wp_pointer_warp_v1` | ✅ |
| `wp_pointer_gestures_v1` | ✅ |
| `wp_drm_lease_v1` | ✅ |
| `wp_security_context_v1` | ✅ |
| `wp_content_type_v1` | ✅ |
| `wp_alpha_modifier_v1` | ✅ |
| `ext_background_effect_v1` | ✅ |
| `xdg_system_bell_v1` | ✅ |
