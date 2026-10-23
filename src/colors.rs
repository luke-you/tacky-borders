use anyhow::{Context, anyhow};
use core::f32;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F, D2D1_GRADIENT_STOP};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BRUSH_PROPERTIES, D2D1_EXTEND_MODE_CLAMP, D2D1_GAMMA_2_2,
    D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, ID2D1Brush, ID2D1LinearGradientBrush, ID2D1RenderTarget,
    ID2D1SolidColorBrush,
};
use windows_numerics::{Matrix3x2, Vector2};

use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use crate::theme::is_light_theme;
use crate::utils::WindowsCompatibleResult;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ColorBrushConfig {
    Solid(String),
    Gradient(GradientBrushConfig),
    ThemeAware(ThemeAwareColor),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ThemeAwareColor {
    pub dark: Box<ColorBrushConfig>,
    pub light: Box<ColorBrushConfig>,
}

impl Default for ColorBrushConfig {
    fn default() -> Self {
        Self::Solid("accent".to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GradientBrushConfig {
    pub colors: Vec<String>,
    pub direction: GradientDirection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum GradientDirection {
    Angle(String),
    Coordinates(GradientCoordinates),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GradientCoordinates {
    pub start: [f32; 2],
    pub end: [f32; 2],
}

#[derive(Debug, Clone)]
pub enum ColorBrush {
    Solid(SolidBrush),
    Gradient(GradientBrush),
}

impl Default for ColorBrush {
    fn default() -> Self {
        ColorBrush::Solid(SolidBrush {
            color: D2D1_COLOR_F::default(),
            brush: None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SolidBrush {
    color: D2D1_COLOR_F,
    brush: Option<ID2D1SolidColorBrush>,
}

#[derive(Debug, Clone)]
pub struct GradientBrush {
    gradient_stops: Vec<D2D1_GRADIENT_STOP>,
    direction: GradientCoordinates,
    brush: Option<ID2D1LinearGradientBrush>,
}

impl ColorBrushConfig {
    pub fn to_color_brush(&self, is_active_color: bool) -> ColorBrush {
        match self {
            ColorBrushConfig::ThemeAware(theme) => {
                let resolved = if is_light_theme() {
                    &theme.light
                } else {
                    &theme.dark
                };
                resolved.to_color_brush(is_active_color)
            }
            ColorBrushConfig::Solid(solid_config) => {
                if solid_config == "accent" {
                    ColorBrush::Solid(SolidBrush {
                        color: get_accent_color(is_active_color),
                        brush: None,
                    })
                } else {
                    ColorBrush::Solid(SolidBrush {
                        color: get_color_from_hex(solid_config.as_str()),
                        brush: None,
                    })
                }
            }
            ColorBrushConfig::Gradient(gradient_config) => {
                // We use 'step' to calculate the position of each color in the gradient below
                let step = 1.0 / (gradient_config.colors.len() - 1) as f32;

                let gradient_stops = gradient_config
                    .clone()
                    .colors
                    .into_iter()
                    .enumerate()
                    .map(|(i, color)| D2D1_GRADIENT_STOP {
                        position: i as f32 * step,
                        color: if color == "accent" {
                            get_accent_color(is_active_color)
                        } else {
                            get_color_from_hex(color.as_str())
                        },
                    })
                    .collect();

                let direction = match gradient_config.direction {
                    GradientDirection::Angle(ref angle) => {
                        let Some(degree) = angle
                            .strip_suffix("deg")
                            .and_then(|d| d.trim().parse::<f32>().ok())
                        else {
                            error!("config contains an invalid gradient direction!");
                            return ColorBrush::default();
                        };

                        // Calculate x and y distance from the center point [0.5, 0.5].
                        // We multiply rad.sin() by -1 (flip the y-component) because Direct2D uses
                        // the top-left as the origin, but most people expect bottom-left.
                        let rad = degree as f64 * PI / 180.0;
                        let (x_raw, y_raw) = (rad.cos(), -rad.sin());
                        let scalar = (1.0 / f64::max(x_raw.abs(), y_raw.abs())) * 0.5;
                        let (x_dist, y_dist) = (x_raw * scalar, y_raw * scalar);

                        let start = [0.5 - x_dist as f32, 0.5 - y_dist as f32];
                        let end = [0.5 + x_dist as f32, 0.5 + y_dist as f32];

                        GradientCoordinates { start, end }
                    }
                    GradientDirection::Coordinates(ref coordinates) => coordinates.clone(),
                };

                ColorBrush::Gradient(GradientBrush {
                    gradient_stops,
                    direction,
                    brush: None,
                })
            }
        }
    }
}

impl ColorBrush {
    // NOTE: ID2D1DeviceContext implements From<&ID2D1DeviceContext> for &ID2D1RenderTarget
    pub fn init_brush(
        &mut self,
        renderer: &ID2D1RenderTarget,
        bounds: &D2D_RECT_F,
        brush_properties: &D2D1_BRUSH_PROPERTIES,
    ) -> WindowsCompatibleResult<()> {
        match self {
            ColorBrush::Solid(solid) => unsafe {
                let id2d1_brush =
                    renderer.CreateSolidColorBrush(&solid.color, Some(brush_properties))?;

                solid.brush = Some(id2d1_brush);

                Ok(())
            },
            ColorBrush::Gradient(gradient) => unsafe {
                let gradient_properties = gradient.linear_gradient_properties(bounds);

                let gradient_stop_collection = renderer.CreateGradientStopCollection(
                    &gradient.gradient_stops,
                    D2D1_GAMMA_2_2,
                    D2D1_EXTEND_MODE_CLAMP,
                )?;

                let id2d1_brush = renderer.CreateLinearGradientBrush(
                    &gradient_properties,
                    Some(brush_properties),
                    &gradient_stop_collection,
                )?;

                gradient.brush = Some(id2d1_brush);

                Ok(())
            },
        }
    }

    pub fn get_brush(&self) -> Option<&ID2D1Brush> {
        match self {
            ColorBrush::Solid(solid) => solid.brush.as_ref().map(|id2d1_brush| id2d1_brush.into()),
            ColorBrush::Gradient(gradient) => gradient
                .brush
                .as_ref()
                .map(|id2d1_brush| id2d1_brush.into()),
        }
    }

    pub fn set_opacity(&self, opacity: f32) -> anyhow::Result<()> {
        match self {
            ColorBrush::Solid(solid) => {
                let id2d1_brush = solid
                    .brush
                    .as_ref()
                    .context("brush has not been created yet")?;

                unsafe { id2d1_brush.SetOpacity(opacity) };
            }
            ColorBrush::Gradient(gradient) => {
                let id2d1_brush = gradient
                    .brush
                    .as_ref()
                    .context("brush has not been created yet")?;

                unsafe { id2d1_brush.SetOpacity(opacity) };
            }
        }

        Ok(())
    }

    pub fn get_opacity(&self) -> anyhow::Result<f32> {
        match self {
            ColorBrush::Solid(solid) => {
                let id2d1_brush = solid
                    .brush
                    .as_ref()
                    .context("brush has not been created yet")?;

                Ok(unsafe { id2d1_brush.GetOpacity() })
            }
            ColorBrush::Gradient(gradient) => {
                let id2d1_brush = gradient
                    .brush
                    .as_ref()
                    .context("brush has not been created yet")?;

                Ok(unsafe { id2d1_brush.GetOpacity() })
            }
        }
    }

    pub fn set_transform(&self, transform: &Matrix3x2) {
        match self {
            ColorBrush::Solid(solid) => {
                if let Some(ref id2d1_brush) = solid.brush {
                    unsafe { id2d1_brush.SetTransform(transform) };
                }
            }
            ColorBrush::Gradient(gradient) => {
                if let Some(ref id2d1_brush) = gradient.brush {
                    unsafe { id2d1_brush.SetTransform(transform) };
                }
            }
        }
    }

    pub fn get_transform(&self) -> Option<Matrix3x2> {
        match self {
            ColorBrush::Solid(solid) => solid.brush.as_ref().map(|id2d1_brush| {
                let mut transform = Matrix3x2::default();
                unsafe { id2d1_brush.GetTransform(&mut transform) };

                transform
            }),
            ColorBrush::Gradient(gradient) => gradient.brush.as_ref().map(|id2d1_brush| {
                let mut transform = Matrix3x2::default();
                unsafe { id2d1_brush.GetTransform(&mut transform) };

                transform
            }),
        }
    }
}

impl GradientBrush {
    /// Converts normalized gradient direction coordinates into bitmap-space start/end points.
    fn linear_gradient_properties(
        &self,
        bounds: &D2D_RECT_F,
    ) -> D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;

        D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
            startPoint: Vector2 {
                X: bounds.left + (self.direction.start[0] * width),
                Y: bounds.top + (self.direction.start[1] * height),
            },
            endPoint: Vector2 {
                X: bounds.left + (self.direction.end[0] * width),
                Y: bounds.top + (self.direction.end[1] * height),
            },
        }
    }

    pub fn update_start_end_points(&self, bounds: &D2D_RECT_F) {
        let gradient_properties = self.linear_gradient_properties(bounds);

        if let Some(ref id2d1_brush) = self.brush {
            unsafe {
                id2d1_brush.SetStartPoint(gradient_properties.startPoint);
                id2d1_brush.SetEndPoint(gradient_properties.endPoint)
            };
        }
    }
}

fn get_accent_color(is_active_color: bool) -> D2D1_COLOR_F {
    const DWM_SUBKEY: &str = r"SOFTWARE\Microsoft\Windows\DWM";
    const ACCENT_COLOR_VALUE: &str = "AccentColor";

    let raw = match RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(DWM_SUBKEY)
        .and_then(|key| key.get_value::<u32, _>(ACCENT_COLOR_VALUE))
    {
        Ok(val) => {
            // AccentColor is stored in 0xAABBGGRR (BGR) order.
            let b = ((val & 0x00FF0000) >> 16) as f32 / 255.0;
            let g = ((val & 0x0000FF00) >> 8) as f32 / 255.0;
            let r = (val & 0x000000FF) as f32 / 255.0;
            D2D1_COLOR_F { r, g, b, a: 1.0 }
        }
        Err(e) => {
            error!("could not read AccentColor from registry: {e}");
            D2D1_COLOR_F::default()
        }
    };

    if is_active_color {
        raw
    } else {
        let avg = (raw.r + raw.g + raw.b) / 3.0;
        D2D1_COLOR_F {
            r: avg / 1.5 + raw.r / 10.0,
            g: avg / 1.5 + raw.g / 10.0,
            b: avg / 1.5 + raw.b / 10.0,
            a: 1.0,
        }
    }
}

fn get_color_from_hex(hex: &str) -> D2D1_COLOR_F {
    let s = hex.strip_prefix("#").unwrap_or_default();
    parse_hex(s).unwrap_or_else(|err| {
        error!("could not parse hex: {err:#}");
        D2D1_COLOR_F::default()
    })
}

fn parse_hex(s: &str) -> anyhow::Result<D2D1_COLOR_F> {
    if !matches!(s.len(), 3 | 4 | 6 | 8) || !s[1..].chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("invalid hex: {s}"));
    }

    let n = s.len();

    let parse_digit = |digit: &str, single: bool| -> anyhow::Result<f32> {
        u8::from_str_radix(digit, 16)
            .map(|n| {
                if single {
                    ((n << 4) | n) as f32 / 255.0
                } else {
                    n as f32 / 255.0
                }
            })
            .map_err(|_| anyhow!("invalid hex: {s}"))
    };

    if n == 3 || n == 4 {
        let r = parse_digit(&s[0..1], true)?;
        let g = parse_digit(&s[1..2], true)?;
        let b = parse_digit(&s[2..3], true)?;

        let a = if n == 4 {
            parse_digit(&s[3..4], true)?
        } else {
            1.0
        };

        Ok(D2D1_COLOR_F { r, g, b, a })
    } else if n == 6 || n == 8 {
        let r = parse_digit(&s[0..2], false)?;
        let g = parse_digit(&s[2..4], false)?;
        let b = parse_digit(&s[4..6], false)?;

        let a = if n == 8 {
            parse_digit(&s[6..8], false)?
        } else {
            1.0
        };

        Ok(D2D1_COLOR_F { r, g, b, a })
    } else {
        Err(anyhow!("invalid hex: {s}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vertical_gradient_90() -> anyhow::Result<()> {
        let color_brush_config = ColorBrushConfig::Gradient(GradientBrushConfig {
            colors: vec!["#ffffff".to_string(), "#000000".to_string()],
            direction: GradientDirection::Angle("90deg".to_string()),
        });
        let color_brush = color_brush_config.to_color_brush(true);

        if let ColorBrush::Gradient(ref gradient) = color_brush {
            assert!(gradient.direction.start == [0.5, 1.0]);
            assert!(gradient.direction.end == [0.5, 0.0]);
        } else {
            panic!("created incorrect color brush");
        }

        Ok(())
    }

    #[test]
    fn test_vertical_gradient_neg90() -> anyhow::Result<()> {
        let color_brush_config = ColorBrushConfig::Gradient(GradientBrushConfig {
            colors: vec!["#ffffff".to_string(), "#000000".to_string()],
            direction: GradientDirection::Angle("-90deg".to_string()),
        });
        let color_brush = color_brush_config.to_color_brush(true);

        if let ColorBrush::Gradient(ref gradient) = color_brush {
            assert!(gradient.direction.start == [0.5, 0.0]);
            assert!(gradient.direction.end == [0.5, 1.0]);
        } else {
            panic!("created incorrect color brush");
        }

        Ok(())
    }

    #[test]
    fn test_gradient_excess_angle() -> anyhow::Result<()> {
        let color_brush_config = ColorBrushConfig::Gradient(GradientBrushConfig {
            colors: vec!["#ffffff".to_string(), "#000000".to_string()],
            direction: GradientDirection::Angle("-540deg".to_string()),
        });
        let color_brush = color_brush_config.to_color_brush(true);

        if let ColorBrush::Gradient(ref gradient) = color_brush {
            assert!(gradient.direction.start == [1.0, 0.5]);
            assert!(gradient.direction.end == [0.0, 0.5]);
        } else {
            panic!("created incorrect color brush");
        }

        Ok(())
    }

    #[test]
    fn test_color_parser_translucent() -> anyhow::Result<()> {
        let color_brush_config = ColorBrushConfig::Solid("#ffffff80".to_string());
        let color_brush = color_brush_config.to_color_brush(true);

        if let ColorBrush::Solid(ref solid) = color_brush {
            assert!(
                solid.color
                    == D2D1_COLOR_F {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 128.0 / 255.0
                    }
            );
        } else {
            panic!("created incorrect color brush");
        }

        Ok(())
    }
}
