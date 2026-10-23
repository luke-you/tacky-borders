use anyhow::Context;
use serde::Deserialize;
use std::slice;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_COMPOSITE_MODE_DESTINATION_OUT, D2D1_COMPOSITE_MODE_SOURCE_OVER,
};
use windows::Win32::Graphics::Direct2D::{
    CLSID_D2D1ColorMatrix, CLSID_D2D1Composite, CLSID_D2D1GaussianBlur, CLSID_D2D1Morphology,
    CLSID_D2D1Opacity, CLSID_D2D12DAffineTransform, D2D1_2DAFFINETRANSFORM_PROP_TRANSFORM_MATRIX,
    D2D1_COLORMATRIX_PROP_COLOR_MATRIX, D2D1_DIRECTIONALBLUR_OPTIMIZATION_SPEED,
    D2D1_GAUSSIANBLUR_PROP_OPTIMIZATION, D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION,
    D2D1_INTERPOLATION_MODE_LINEAR, D2D1_MORPHOLOGY_MODE_DILATE, D2D1_MORPHOLOGY_PROP_HEIGHT,
    D2D1_MORPHOLOGY_PROP_MODE, D2D1_MORPHOLOGY_PROP_WIDTH, D2D1_OPACITY_PROP_OPACITY,
    D2D1_PROPERTY_TYPE_ENUM, D2D1_PROPERTY_TYPE_FLOAT, D2D1_PROPERTY_TYPE_MATRIX_3X2,
    D2D1_PROPERTY_TYPE_MATRIX_5X4, D2D1_PROPERTY_TYPE_UINT32, ID2D1CommandList, ID2D1Effect,
};
use windows_numerics::Matrix3x2;

use crate::config::{serde_default_bool, serde_default_f32};
use crate::render_backend::RenderBackend;
use crate::utils::{
    StandaloneWindowsError, T_E_ERROR, T_E_UNINIT, ToWindowsResult, WindowsCompatibleError,
    WindowsCompatibleResult, WindowsContext,
};
use crate::window_border::WindowState;

#[derive(Debug, Default, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EffectsConfig {
    #[serde(default)]
    active: Vec<EffectParamsConfig>,
    #[serde(default)]
    inactive: Vec<EffectParamsConfig>,
    #[serde(default = "serde_default_bool::<true>")]
    enabled: bool,
}

impl EffectsConfig {
    pub fn to_effects(&self, dpi: u32) -> Effects {
        if self.enabled {
            Effects {
                active: self
                    .active
                    .iter()
                    .map(|config| config.to_effect_params(dpi))
                    .collect(),
                inactive: self
                    .inactive
                    .iter()
                    .map(|config| config.to_effect_params(dpi))
                    .collect(),
                ..Default::default()
            }
        } else {
            Effects::default()
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Effects {
    pub active: Vec<EffectParams>,
    pub inactive: Vec<EffectParams>,
    active_command_list: Option<ID2D1CommandList>,
    inactive_command_list: Option<ID2D1CommandList>,
}

impl Effects {
    pub fn is_enabled(&self) -> bool {
        !self.active.is_empty() || !self.inactive.is_empty()
    }

    pub fn get_current_vec(&self, window_state: WindowState) -> &Vec<EffectParams> {
        match window_state {
            WindowState::Active => &self.active,
            WindowState::Inactive => &self.inactive,
        }
    }

    pub fn should_apply(&self, window_state: WindowState) -> bool {
        !self.get_current_vec(window_state).is_empty()
    }

    pub fn init_command_lists_if_enabled(
        &mut self,
        render_backend: &RenderBackend,
    ) -> WindowsCompatibleResult<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let RenderBackend::V2(backend) = render_backend else {
            return Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(T_E_ERROR, "render backend is not V2"),
            ));
        };
        let d2d_context = &backend.d2d_context;
        let border_bitmap = backend
            .border_bitmap
            .as_ref()
            .context("could not get border_bitmap")
            .to_windows_result(T_E_UNINIT)?;
        let mask_bitmap = backend
            .mask_bitmap
            .as_ref()
            .context("could not get mask_bitmap")
            .to_windows_result(T_E_UNINIT)?;

        let create_single_list =
            |effect_params_vec: &Vec<EffectParams>| -> WindowsCompatibleResult<ID2D1CommandList> {
                unsafe {
                    // Open a command list to record draw operations
                    let command_list = d2d_context
                        .CreateCommandList()
                        .windows_context("d2d_context.CreateCommandList()")?;

                    d2d_context.SetTarget(&command_list);

                    // Create a vec to store the output effects
                    let mut effects_vec: Vec<ID2D1Effect> = Vec::new();

                    for effect_params in effect_params_vec.iter() {
                        let effect = match effect_params.effect_type {
                            EffectType::Glow => {
                                // Create the blur effect first
                                let blur_effect = d2d_context
                                    .CreateEffect(&CLSID_D2D1GaussianBlur)
                                    .windows_context("glow_blur_effect")?;

                                // If spread > 0, apply morphology dilation before blur
                                // This preserves corner shapes while expanding the glow
                                //
                                // TODO: Find more efficient method of rendering spread (maybe
                                // just expand border_outer_rect in BorderDrawer::render())
                                if effect_params.spread > 0.0 {
                                    let morphology_effect = d2d_context
                                        .CreateEffect(&CLSID_D2D1Morphology)
                                        .windows_context("glow_morphology_effect")?;
                                    morphology_effect.SetInput(0, border_bitmap, false);

                                    // Set morphology mode to dilate (expand)
                                    morphology_effect
                                        .SetValue(
                                            D2D1_MORPHOLOGY_PROP_MODE.0 as u32,
                                            D2D1_PROPERTY_TYPE_ENUM,
                                            &D2D1_MORPHOLOGY_MODE_DILATE.0.to_le_bytes(),
                                        )
                                        .windows_context(
                                            "glow_morphology_effect.SetValue() mode",
                                        )?;

                                    // Set dilation size based on spread
                                    let dilation_size =
                                        (effect_params.spread.round() as u32).max(1);
                                    morphology_effect
                                        .SetValue(
                                            D2D1_MORPHOLOGY_PROP_WIDTH.0 as u32,
                                            D2D1_PROPERTY_TYPE_UINT32,
                                            &dilation_size.to_le_bytes(),
                                        )
                                        .windows_context(
                                            "glow_morphology_effect.SetValue() width",
                                        )?;
                                    morphology_effect
                                        .SetValue(
                                            D2D1_MORPHOLOGY_PROP_HEIGHT.0 as u32,
                                            D2D1_PROPERTY_TYPE_UINT32,
                                            &dilation_size.to_le_bytes(),
                                        )
                                        .windows_context(
                                            "glow_morphology_effect.SetValue() height",
                                        )?;

                                    // Chain blur to morphology output
                                    blur_effect.SetInput(
                                        0,
                                        &morphology_effect.GetOutput().windows_context(
                                            "could not get glow morphology output",
                                        )?,
                                        false,
                                    );
                                } else {
                                    // No spread - just blur the border directly
                                    blur_effect.SetInput(0, border_bitmap, false);
                                }

                                // Apply blur using config's std_dev as-is
                                blur_effect
                                    .SetValue(
                                        D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION.0 as u32,
                                        D2D1_PROPERTY_TYPE_FLOAT,
                                        &effect_params.std_dev.to_le_bytes(),
                                    )
                                    .windows_context("glow_blur_effect.SetValue() std deviation")?;
                                blur_effect
                                    .SetValue(
                                        D2D1_GAUSSIANBLUR_PROP_OPTIMIZATION.0 as u32,
                                        D2D1_PROPERTY_TYPE_ENUM,
                                        &D2D1_DIRECTIONALBLUR_OPTIMIZATION_SPEED.0.to_le_bytes(),
                                    )
                                    .windows_context("glow_blur_effect.SetValue() optimization")?;

                                blur_effect
                            }
                            EffectType::Shadow => {
                                // Create the color matrix effect to convert to black
                                let color_matrix_effect = d2d_context
                                    .CreateEffect(&CLSID_D2D1ColorMatrix)
                                    .windows_context("color_matrix_effect")?;

                                // 5x4 matrix in row-major order: sets RGB to 0, keeps A
                                #[rustfmt::skip]
                                let black_matrix: [f32; 20] = [
                                    0.0, 0.0, 0.0, 0.0,  // R row
                                    0.0, 0.0, 0.0, 0.0,  // G row
                                    0.0, 0.0, 0.0, 0.0,  // B row
                                    0.0, 0.0, 0.0, 1.0,  // A row (preserve alpha)
                                    0.0, 0.0, 0.0, 0.0,  // Offset row
                                ];
                                let matrix_bytes: &[u8] = slice::from_raw_parts(
                                    black_matrix.as_ptr() as *const u8,
                                    size_of::<[f32; 20]>(),
                                );
                                color_matrix_effect
                                    .SetValue(
                                        D2D1_COLORMATRIX_PROP_COLOR_MATRIX.0 as u32,
                                        D2D1_PROPERTY_TYPE_MATRIX_5X4,
                                        matrix_bytes,
                                    )
                                    .windows_context("color_matrix_effect.SetValue()")?;

                                // If spread > 0, apply morphology dilation
                                // This preserves corner shapes while expanding the shadow
                                if effect_params.spread > 0.0 {
                                    let morphology_effect = d2d_context
                                        .CreateEffect(&CLSID_D2D1Morphology)
                                        .windows_context("shadow_morphology_effect")?;
                                    morphology_effect.SetInput(0, border_bitmap, false);

                                    // Set morphology mode to dilate (expand)
                                    morphology_effect
                                        .SetValue(
                                            D2D1_MORPHOLOGY_PROP_MODE.0 as u32,
                                            D2D1_PROPERTY_TYPE_ENUM,
                                            &D2D1_MORPHOLOGY_MODE_DILATE.0.to_le_bytes(),
                                        )
                                        .windows_context(
                                            "shadow_morphology_effect.SetValue() mode",
                                        )?;

                                    // Set dilation size based on spread
                                    let dilation_size =
                                        (effect_params.spread.round() as u32).max(1);
                                    morphology_effect
                                        .SetValue(
                                            D2D1_MORPHOLOGY_PROP_WIDTH.0 as u32,
                                            D2D1_PROPERTY_TYPE_UINT32,
                                            &dilation_size.to_le_bytes(),
                                        )
                                        .windows_context(
                                            "shadow_morphology_effect.SetValue() width",
                                        )?;
                                    morphology_effect
                                        .SetValue(
                                            D2D1_MORPHOLOGY_PROP_HEIGHT.0 as u32,
                                            D2D1_PROPERTY_TYPE_UINT32,
                                            &dilation_size.to_le_bytes(),
                                        )
                                        .windows_context(
                                            "shadow_morphology_effect.SetValue() height",
                                        )?;

                                    // Chain color matrix to morphology output
                                    color_matrix_effect.SetInput(
                                        0,
                                        &morphology_effect.GetOutput().windows_context(
                                            "could not get shadow morphology output",
                                        )?,
                                        false,
                                    );
                                } else {
                                    // No spread - just apply color matrix to border directly
                                    color_matrix_effect.SetInput(0, border_bitmap, false);
                                }

                                // Create blur effect
                                let blur_effect = d2d_context
                                    .CreateEffect(&CLSID_D2D1GaussianBlur)
                                    .windows_context("shadow_blur_effect")?;
                                blur_effect.SetInput(
                                    0,
                                    &color_matrix_effect
                                        .GetOutput()
                                        .windows_context("could not get color_matrix output")?,
                                    false,
                                );

                                // Apply blur using param's std_dev
                                blur_effect
                                    .SetValue(
                                        D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION.0 as u32,
                                        D2D1_PROPERTY_TYPE_FLOAT,
                                        &effect_params.std_dev.to_le_bytes(),
                                    )
                                    .windows_context(
                                        "shadow_blur_effect.SetValue() std deviation",
                                    )?;
                                blur_effect
                                    .SetValue(
                                        D2D1_GAUSSIANBLUR_PROP_OPTIMIZATION.0 as u32,
                                        D2D1_PROPERTY_TYPE_ENUM,
                                        &D2D1_DIRECTIONALBLUR_OPTIMIZATION_SPEED.0.to_le_bytes(),
                                    )
                                    .windows_context(
                                        "shadow_blur_effect.SetValue() optimization",
                                    )?;

                                blur_effect
                            }
                        };

                        let effect_with_opacity = d2d_context
                            .CreateEffect(&CLSID_D2D1Opacity)
                            .windows_context("effect_with_opacity")?;
                        effect_with_opacity.SetInput(
                            0,
                            &effect
                                .GetOutput()
                                .windows_context("could not get _ effect output")?,
                            false,
                        );
                        effect_with_opacity
                            .SetValue(
                                D2D1_OPACITY_PROP_OPACITY.0 as u32,
                                D2D1_PROPERTY_TYPE_FLOAT,
                                &effect_params.opacity.to_le_bytes(),
                            )
                            .windows_context("effect_with_opacity.SetValue()")?;

                        let effect_with_opacity_translation = d2d_context
                            .CreateEffect(&CLSID_D2D12DAffineTransform)
                            .windows_context("effect_with_opacity_translation")?;
                        effect_with_opacity_translation.SetInput(
                            0,
                            &effect_with_opacity
                                .GetOutput()
                                .windows_context("could not get effect_with_opacity output")?,
                            false,
                        );
                        let translation_matrix = Matrix3x2::translation(
                            effect_params.translation.x,
                            effect_params.translation.y,
                        );
                        let translation_matrix_bytes: &[u8] = slice::from_raw_parts(
                            &translation_matrix as *const Matrix3x2 as *const u8,
                            size_of::<Matrix3x2>(),
                        );
                        effect_with_opacity_translation
                            .SetValue(
                                D2D1_2DAFFINETRANSFORM_PROP_TRANSFORM_MATRIX.0 as u32,
                                D2D1_PROPERTY_TYPE_MATRIX_3X2,
                                translation_matrix_bytes,
                            )
                            .windows_context("effect_with_opacity_translation.SetValue()")?;

                        effects_vec.push(effect_with_opacity_translation);
                    }

                    // Create a composite effect and link it to the above effects
                    let composite_effect = d2d_context
                        .CreateEffect(&CLSID_D2D1Composite)
                        .windows_context("composite_effect")?;
                    composite_effect
                        .SetInputCount(effects_vec.len() as u32 + 1)
                        .windows_context("could not set composite effect input count")?;

                    for (index, effect) in effects_vec.iter().enumerate() {
                        composite_effect.SetInput(
                            index as u32,
                            &effect
                                .GetOutput()
                                .windows_context(format!("could not get effect output: {index}"))?,
                            false,
                        );
                    }
                    composite_effect.SetInput(effects_vec.len() as u32, border_bitmap, false);

                    // Begin recording commands to the command list
                    d2d_context.BeginDraw();
                    d2d_context.Clear(None);

                    d2d_context.DrawImage(
                        &composite_effect
                            .GetOutput()
                            .windows_context("could not get composite output")?,
                        None,
                        None,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        D2D1_COMPOSITE_MODE_SOURCE_OVER,
                    );

                    // We use COMPOSITE_MODE_DESTINATION_OUT to inverse mask out the inner rect
                    d2d_context.DrawImage(
                        mask_bitmap,
                        None,
                        None,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        D2D1_COMPOSITE_MODE_DESTINATION_OUT,
                    );

                    d2d_context.EndDraw(None, None)?;

                    // Close the command list to tell it we are done recording
                    command_list
                        .Close()
                        .windows_context("command_list.Close()")?;

                    Ok(command_list)
                }
            };

        let active_command_list =
            create_single_list(&self.active).windows_context("active_command_list")?;
        let inactive_command_list =
            create_single_list(&self.inactive).windows_context("inactive_command_list")?;

        self.active_command_list = Some(active_command_list);
        self.inactive_command_list = Some(inactive_command_list);

        Ok(())
    }

    pub fn get_current_command_list(
        &self,
        window_state: WindowState,
    ) -> anyhow::Result<&ID2D1CommandList> {
        match window_state {
            WindowState::Active => self
                .active_command_list
                .as_ref()
                .context("could not get active_command_list"),
            WindowState::Inactive => self
                .inactive_command_list
                .as_ref()
                .context("could not get inactive_command_list"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EffectParamsConfig {
    #[serde(alias = "type")]
    effect_type: EffectType,
    #[serde(alias = "blur")]
    #[serde(alias = "radius")] // Backwards compatibility
    #[serde(default = "serde_default_f32::<8>")]
    std_dev: f32,
    #[serde(default)]
    spread: f32,
    #[serde(default = "serde_default_f32::<1>")]
    opacity: f32,
    #[serde(default)]
    translation: Translation,
}

impl EffectParamsConfig {
    pub fn to_effect_params(&self, dpi: u32) -> EffectParams {
        const BASE_DPI: f32 = 96.0;
        let scale = dpi as f32 / BASE_DPI;

        EffectParams {
            effect_type: self.effect_type,
            opacity: self.opacity,
            std_dev: self.std_dev * scale,
            spread: self.spread * scale,
            translation: Translation {
                x: self.translation.x * scale,
                y: self.translation.y * scale,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct EffectParams {
    pub effect_type: EffectType,
    pub std_dev: f32,
    pub spread: f32,
    pub opacity: f32,
    pub translation: Translation,
}

impl EffectParams {
    /// Padding needed on each side of a window so as to not clip the effect
    pub fn required_padding(&self) -> i32 {
        let std_dev = self.std_dev;
        let spread = self.spread;
        let max_translation = f32::max(self.translation.x.abs(), self.translation.y.abs());

        // 3 standard deviations gets us 99.7% coverage, which should be good enough
        ((std_dev * 3.0) + spread + max_translation).ceil() as i32
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
pub enum EffectType {
    Glow,
    Shadow,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq)]
pub struct Translation {
    pub x: f32,
    pub y: f32,
}
