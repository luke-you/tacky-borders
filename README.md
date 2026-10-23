# tacky-borders

![image](https://github.com/user-attachments/assets/37754479-2311-400e-b019-e0ad1db642a9)
_tacky-borders_ lets you customize window borders on Windows 10 and 11.

## Installation

### Pre-built Release

The easiest way to install _tacky-borders_ is to download a pre-built release from the [releases](https://github.com/luke-you/tacky-borders/releases) page.

When you run the .exe for the first time, _tacky-borders_ will automatically generate a config file for you in ```%userprofile%/.config/tacky-borders/```.

### Build It Yourself

Alternatively, if you wish to build it yourself, you can follow these steps:

1. Install the necessary tools:
   - [Rust](https://www.rust-lang.org/tools/install)
   - [MSVC build tools](https://visualstudio.microsoft.com/downloads/)
2. Clone the repository:

   ```sh
   git clone https://github.com/luke-you/tacky-borders.git
   ```

3. Navigate to the project directory:

   ```sh
   cd tacky-borders
   ```

3. Build or run the project:

   ```sh
   cargo build --release
   ```

   or

   ```sh
   cargo run --release
   ```

## Uninstallation

To uninstall, it's as easy as deleting `tacky-borders.exe`.

> [!NOTE]
> If you wish to remove all traces of _tacky-borders_ from your system, you should also ensure auto-start is disabled via the tray menu and delete the config folder located at ```%userprofile%/.config/tacky-borders/```

## Configuration Options

The config file is located in ```%userprofile%/.config/tacky-borders/```. You can easily access this folder by right clicking on the tray icon and hitting "Show Config"

The following auto-generated config.yaml is included as reference:

```yaml
# watch_config_changes: Automatically reload borders whenever the config file is modified.
watch_config_changes: True

# enable_logging: Log messages (info, warnings, and errors) to a file.
# NOTE: Changes require an application restart to take effect.
enable_logging: True

# enable_ipc_server: Handle runtime IPC commands sent through the CLI.
enable_ipc_server: True

# rendering_backend: Type of renderer. Supported values:
#   - V2: A more complex, feature-rich renderer. Available in v1.2.0 and above.
#   - Legacy: A simpler, more limited renderer. Available in v0.1.0 and above.
#
# NOTE: Performance between the two may vary system-to-system. Choose whichever works best.
rendering_backend: V2

# Global configuration options
global:
  # border_width: Width of the border (in pixels)
  border_width: 3

  # border_offset: Offset of the border from the window edges (in pixels)
  #   - Negative values shrink the border inwards
  #   - Positive values expand the border outwards
  border_offset: -1

  # border-radius: Radius of the border's corners. Supported values:
  #   - Auto: Automatically determine the radius
  #   - Square: Sharp corners (radius = 0)
  #   - Round: Fully rounded corners
  #   - RoundSmall: Slightly rounded corners
  #   - Or specify any numeric value for a custom radius
  border_radius: Auto

  # border_z_order: Z-order of the border relative to its window. Supported values:
  #   - AboveWindow: Place the border above its window
  #   - BelowWindow: Place the border below its window
  #
  # NOTE: Using 'AboveWindow' may cause the border to flicker when used with some applications
  # that manipulate window layering, leading to z-order conflicts.
  border_z_order: AboveWindow

  # follow_native_border: Follow Windows' behavior for border visibility.
  #
  # Windows already shows or hides native borders depending on a window's type or state
  # (e.g., they're hidden for borderless fullscreen windows). When enabled, this option makes
  # tacky-borders follow that visibility behavior.
  #
  # NOTE: This setting only affects visibility for borders that have been initialized.
  # To make sure a border is initialized in the first place, use window_rules.
  follow_native_border: True

  # initialize_delay: Time (in ms) before the border appears after opening a new window
  # unminimize_delay: Time (in ms) before the border appears after unminimizing a window
  #
  # These settings help accommodate window animations (e.g., open or unminimize animations).
  # Set these to 0 if window animations are disabled.
  #
  # These can also be used to accommodate border animations (e.g., fade animations).
  initialize_delay: 200
  unminimize_delay: 150

  # active_color: the color of the active window's border
  # inactive_color: the color of the inactive window's border
  #
  # Supported color types:
  #   - Solid: Use a hex code or "accent"
  #       Example:
  #         active_color: "#ffffff"
  #         OR
  #         active_color: "accent"
  #       The supported hex formats are: #RGB, #RGBA, #RRGGBB, #RRGGBBAA
  #   - Gradient: Define colors and direction
  #       Example:
  #         active_color:
  #           colors: ["#000000", "#ffffff"]
  #           direction: 45deg
  #         OR
  #         active_color:
  #           colors: ["#000000", "#ffffff"]
  #           direction:
  #             start: [0.0, 1.0]
  #             end: [1.0, 0.0]
  #       NOTE: [0.0, 0.0] = top-left, [1.0, 1.0] = bottom-right
  #   - Theme-aware: Define separate colors for dark/light Windows system themes
  #       Example:
  #         active_color:
  #           dark: "#ffffff"
  #           light: "#000000"
  #       Each value (dark/light) can be a hex code, "accent", or a gradient.
  #       Borders automatically update when the system theme changes.
  active_color:
    colors: ["#6274e7", "#8752a3"]
    direction: 45deg

  inactive_color:
    colors: ["#30304f", "#363c69"]
    direction:
      start: [0.0, 1.0]
      end: [1.0, 0.0]

  # komorebi_colors: Additional integration for komorebi's special window kinds
  #   - active_color is used for komorebi's "Single" window kind
  #   - inactive_color is used for komorebi's "Unfocused" window kind
  komorebi_colors:
    stack_color: "#e762b7"
    monocle_color: "#62e793"
    floating_color: "#f5f5a5"
    enabled: False

  # animations: Configure animation behavior for window borders
  #   active: Animations for active windows
  #   inactive: Animations for inactive windows
  #   fps: Animation frame rate (1 through 1000 when enabled)
  #   duration: Optional positive finite duration in milliseconds when enabled
  # Animation timing fields are ignored when enabled is False.
  #
  # Supported animation types:
  #   - Spiral
  #   - ReverseSpiral
  #   - Fade
  #
  # Specify animation types and parameters as follows:
  #   active:
  #     - type: ReverseSpiral
  #       duration: 1800
  #       easing: Linear
  #
  #     - type: Fade
  #       duration: 200
  #       easing: EaseInOutQuad
  #
  # NOTE: Spiral animations may be resource-intensive on low-end systems.
  animations:
    active:
      - type: ReverseSpiral
        duration: 1800
        easing: Linear

      - type: Fade
        duration: 200
        easing: EaseInOutQuad

    inactive:
      - type: Spiral
        duration: 1800
        easing: Linear

      - type: Fade
        duration: 200
        easing: EaseInOutQuad

    fps: 60
    enabled: False

  # effects: Configure visual effects for window borders
  #   active: Effects for active windows
  #   inactive: Effects for inactive windows
  #
  # Supported effect types:
  #   - Shadow
  #   - Glow
  #
  # Specify effect types and parameters as follows:
  #   active:
  #    - type: Shadow
  #      blur: 2.0
  #      spread: 0.0
  #      opacity: 1.0
  #      translation:
  #        x: 0
  #        y: 20
  #
  #    - type: Glow
  #      blur: 8.0
  #      spread: 0.0
  #      opacity: 1.0
  #
  # NOTE: These effects can significantly increase CPU and GPU usage.
  # Additionally, effects require rendering_backend: V2 to work.
  effects:
    active:
      - type: Shadow
        blur: 2.0
        spread: 0.0
        opacity: 1.0
        translation:
          x: 0
          y: 0

      - type: Glow
        blur: 8.0
        spread: 0.0
        opacity: 1.0

    inactive:
      - type: Shadow
        blur: 2.0
        spread: 0.0
        opacity: 1.0
        translation:
          x: 0
          y: 0

      - type: Glow
        blur: 8.0
        spread: 0.0
        opacity: 1.0

    enabled: False

# Per-application configuration overrides
window_rules:
  - match: Class
    name: "Windows.UI.Core.CoreWindow"
    enabled: False

  - match: Class
    name: "XamlExplorerHostIslandWindow"
    enabled: False

  - match: Process
    name: "tacky-borders"
    enabled: False

  - match: Process
    name: "zebar"
    enabled: False

  - match: Title
    name: "komorebi-bar"
    enabled: False

  - match: Title
    name: "keyviz"
    enabled: False

  - match: Title
    name: "Picture-in-Picture"
    enabled: False

  - match: Process
    name: "seelen-ui"
    enabled: False

  # Example rule:
  # - match: Class                   # Match based on Class, Title, or Process
  #   name: "MozillaWindowClass"     # Class/title/process name to match
  #   strategy: Equals               # Matching strategy: Equals, Contains, or Regex (default: Equals)
  #   enabled: True                  # Enable border initialization: True, False, or Auto (default: Auto)
  #
  # NOTE:
  #   - Any option in the global config can also be defined in window_rules.
  #   - If not defined in a rule, settings will fall back to global config values.
  #   - Setting 'border enabled' to True forces its initialization, but not necessarily its visibility.
  #     If the border still isn't visible, try setting 'follow_native_border' to False.
```

## Community Projects

- [Color-Presets-for-Tacky-Borders](https://github.com/landnthrn/Color-Presets-for-Tacky-Borders) - Additional color presets including color animations, gradients, one side color fades, and static colors.

## Comparison to cute-borders

Here is another great app that achieves similar functionality: <https://github.com/keifufu/cute-borders>. I've taken a lot of inspiration from them and would highly recommend checking them out!

Although both apps aim to customize window borders, they have totally different implementations, each with their own strengths and limitations. Which one you should use boils down to the following:

**Choose _tacky-borders_ if you want**:

- Customizable border width
- Gradient support
- Multiple animation types
- Windows 10 support (not fully tested, but it has been reported to work)

**Choose _cute-borders_ if you want**:

- Stability and performance due to its use of native Windows API for the borders.
