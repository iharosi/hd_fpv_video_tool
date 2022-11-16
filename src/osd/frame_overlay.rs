
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::io::Error as IOError;

use crate::create_path::{CreatePathError, create_path};
use crate::file::{self, HardLinkError};
use crate::image::WriteImageFile;

use super::dji::file::{Frame as OSDFileFrame, ReadError};
use super::dji::file::FrameIndex;
use super::dji::file::Reader as OSDFileReader;
use super::tile_resize::ResizeTiles;
use crate::image::WriteError as ImageWriteError;

use derive_more::From;
use regex::Regex;
use thiserror::Error;
use hd_fpv_osd_font_tool::prelude::*;
use image::{ImageBuffer, Rgba, GenericImage};
use hd_fpv_osd_font_tool::osd::tile;
use indicatif::{ProgressStyle, ParallelProgressIterator};
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};
use super::dji::{Kind as DJIOSDKind, VideoResolutionTooSmallError};
use hd_fpv_osd_font_tool::dimensions::Dimensions as GenericDimensions;
use lazy_static::lazy_static;

pub type VideoResolution = GenericDimensions<u32>;
pub type Resolution = GenericDimensions<u32>;

#[derive(Debug, Error)]
pub enum DrawFrameOverlayError {
    #[error("video resolution {video_resolution} too small to render {osd_kind} OSD kind without scaling")]
    VideoResolutionTooSmallError{ osd_kind: DJIOSDKind, video_resolution: VideoResolution },
}

pub type Image = ImageBuffer<Rgba<u8>, Vec<u8>>;

// pub fn transparent_frame_overlay(kind: &DJIOSDKind) -> Image {
//     let Resolution { width, height } = kind.dimensions_pixels_for_tile_kind(kind.tile_kind());
//     Image::new(width, height)
// }

pub fn format_overlay_frame_file_index(frame_index: FrameIndex) -> String {
    format!("{:010}.png", frame_index)
}

pub fn make_overlay_frame_file_path<P: AsRef<Path>>(dir_path: P, frame_index: FrameIndex) -> PathBuf {
    [dir_path.as_ref().to_str().unwrap(), &format_overlay_frame_file_index(frame_index)].iter().collect()
}

pub fn link_missing_frames<P: AsRef<Path>>(dir_path: P, existing_frame_indices: &BTreeSet<FrameIndex>) -> Result<(), IOError> {
    let existing_frame_indices_vec = existing_frame_indices.iter().collect::<Vec<&FrameIndex>>();
    for indices in existing_frame_indices_vec.windows(2) {
        if let &[lower_index, greater_index] = indices {
            if *greater_index > lower_index + 1 {
                let original_path = make_overlay_frame_file_path(&dir_path, *lower_index);
                for link_to_index in lower_index+1..*greater_index {
                    let copy_path = make_overlay_frame_file_path(&dir_path, link_to_index);
                    #[allow(clippy::needless_borrow)]
                    std::fs::hard_link(&original_path, copy_path)?;
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Error, From)]
pub enum SaveFramesToDirError {
    #[error(transparent)]
    CreatePathError(CreatePathError),
    #[error(transparent)]
    IOError(IOError),
    #[error(transparent)]
    ReadError(ReadError),
    #[error(transparent)]
    ImageWriteError(ImageWriteError),
    #[error(transparent)]
    HardLinkError(HardLinkError),
    #[error("target directory exists")]
    TargetDirectoryExists,
}

#[derive(Debug, Clone, Copy)]
pub enum Scale {
    No,
    Yes {
        minimum_horizontal_margin: u32,
        minimum_vertical_margin: u32,
    }
}

#[derive(Debug, Error)]
#[error("invalid margins format: {0}")]
pub struct InvalidMarginsFormatError(String);

impl TryFrom<&Option<Option<String>>> for Scale {
    type Error = InvalidMarginsFormatError;

    fn try_from(value: &Option<Option<String>>) -> Result<Self, Self::Error> {
        Ok(
            match value {
                Some(margins) => match margins {
                    Some(margins_str) => {
                        lazy_static! {
                            static ref MARGINS_RE: Regex = Regex::new(r"\A(?P<horiz>\d{1,3}):(?P<vert>\d{1,3})\z").unwrap();
                        }
                        match MARGINS_RE.captures(margins_str) {
                            Some(captures) => {
                                let minimum_horizontal_margin = captures.name("horiz").unwrap().as_str().parse().unwrap();
                                let minimum_vertical_margin = captures.name("vert").unwrap().as_str().parse().unwrap();
                                Scale::Yes { minimum_horizontal_margin, minimum_vertical_margin }
                            },
                            None => return Err(InvalidMarginsFormatError(margins_str.to_owned())),
                        }
                    },
                    None => Scale::Yes { minimum_horizontal_margin: 0, minimum_vertical_margin: 0 },
                },
                None => Scale::No,
            }
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TargetResolution {
    Tr720p,
    Tr720p4By3,
    Tr1080p,
    Tr1080p4by3,
    Custom(VideoResolution),
}

impl TargetResolution {
    pub fn dimensions(&self) -> VideoResolution {
        use TargetResolution::*;
        match self {
            Tr720p => VideoResolution::new(1280, 720),
            Tr720p4By3 => VideoResolution::new(960, 720),
            Tr1080p => VideoResolution::new(1920, 1080),
            Tr1080p4by3 => VideoResolution::new(1440, 1080),
            Custom(resolution) => *resolution,
        }
    }
}

#[derive(Debug, Error)]
#[error("invalid resolution format: {0}")]
pub struct InvalidResolutionFormatError(String);

impl TryFrom<&str> for TargetResolution {
    type Error = InvalidResolutionFormatError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        use TargetResolution::*;
        let resolution = match value {
            "720p" => Tr720p,
            "720p4:3" => Tr1080p4by3,
            "1080p" => Tr1080p,
            "1080p4:3" => Tr1080p4by3,
            custom_res_str => {
                lazy_static! {
                    static ref RES_RE: Regex = Regex::new(r"\A(?P<width>\d{1,5})x(?P<height>\d{1,5})\z").unwrap();
                }
                match RES_RE.captures(custom_res_str) {
                    Some(captures) => {
                        let width = captures.name("width").unwrap().as_str().parse().unwrap();
                        let height = captures.name("height").unwrap().as_str().parse().unwrap();
                        Custom(VideoResolution::new(width, height))
                    },
                    None => return Err(InvalidResolutionFormatError(custom_res_str.to_owned())),
                }
            }
        };
        Ok(resolution)
    }
}

pub struct Generator {
    reader: OSDFileReader,
    tile_images: Vec<tile::Image>,
    overlay_resolution: VideoResolution,
}

impl Generator {

    pub fn new(reader: OSDFileReader, tile_set: &TileSet, target_resolution: TargetResolution, scale: Scale) -> Result<Self, DrawFrameOverlayError> {
        let osd_kind = reader.osd_kind();
        let (overlay_resolution, tile_kind, tile_scaling) = match scale {
            Scale::No => {
                let tile_kind = osd_kind.best_kind_of_tiles_to_use_without_scaling(target_resolution.dimensions()).map_err(|error| {
                    let VideoResolutionTooSmallError { osd_kind, video_resolution } = error;
                    DrawFrameOverlayError::VideoResolutionTooSmallError { osd_kind, video_resolution }
                })?;
                (osd_kind.dimensions_pixels_for_tile_kind(tile_kind), tile_kind, None)
            },
            Scale::Yes { minimum_horizontal_margin, minimum_vertical_margin } => {
                let max_resolution = VideoResolution::new(
                    target_resolution.dimensions().width - 2 * minimum_horizontal_margin,
                    target_resolution.dimensions().height - 2 * minimum_vertical_margin,
                );
                let (tile_kind, tile_dimensions, overlay_dimensions) = osd_kind.best_kind_of_tiles_to_use_with_scaling(max_resolution);
                (overlay_dimensions, tile_kind, Some(tile_dimensions))
            },
        };

        let tile_images = match tile_scaling {
            Some(tile_dimensions) => tile_set[tile_kind].as_slice().resized_tiles_par_with_progress(tile_dimensions),
            None => tile_set[tile_kind].iter().map(|tile| tile.image().clone()).collect(),
        };

        let overlay_res_scale =
            (
                (overlay_resolution.width as f64 /target_resolution.dimensions().width as f64) +
                (overlay_resolution.height as f64 / target_resolution.dimensions().height as f64)
            ) / 2.0;

        if overlay_res_scale < 0.8 {
            log::warn!("without scaling the overlay resolution is much smaller than the target video resolution, consider using scaling for better results");
        }

        Ok(Self { reader, tile_images, overlay_resolution })
    }

    pub fn draw_next_frame(&mut self) -> Result<Option<Image>, ReadError> {
        match self.reader.read_frame()? {
            Some(frame) => Ok(Some(self.draw_frame_overlay(&frame).unwrap())),
            None => Ok(None),
        }
    }

    fn transparent_frame_overlay(&self) -> Image {
        Image::new(self.overlay_resolution.width, self.overlay_resolution.height)
    }

    fn draw_frame_overlay(&self, osd_file_frame: &OSDFileFrame) -> Result<Image, DrawFrameOverlayError> {
        let (tiles_width, tiles_height) = self.tile_images.first().unwrap().dimensions();
        let mut image = self.transparent_frame_overlay();
        for (screen_x, screen_y, tile_index) in osd_file_frame.enumerate_tile_indices() {
            image.copy_from(&self.tile_images[tile_index as usize], screen_x as u32 * tiles_width, screen_y as u32 * tiles_height).unwrap();
        }
        Ok(image)
    }

    pub fn save_frames_to_dir<P: AsRef<Path> + std::marker::Sync>(&mut self, path: P, frame_offset: i32) -> Result<(), SaveFramesToDirError> {
        if path.as_ref().exists() {
            return Err(SaveFramesToDirError::TargetDirectoryExists);
        }
        create_path(&path)?;
        log::info!("generating overlay frames and saving into directory: {}", path.as_ref().to_string_lossy());
        let frames = self.reader.frames()?;

        let first_frame_index = frames.iter().position(|frame| (*frame.index() as i32) > -frame_offset).unwrap();
        let frames = &frames[first_frame_index..];
        let first_frame_index = frames.first().unwrap().index();

        let missing_frames = frame_offset + *first_frame_index as i32;

        // we are missing frames at the beginning
        if missing_frames > 0 {
            log::debug!("Generating blank frames 0..{}", missing_frames - 1);
            let frame_0_path = make_overlay_frame_file_path(&path, 0);
            self.transparent_frame_overlay().write_image_file(&frame_0_path)?;
            for frame_index in 1..missing_frames {
                file::hard_link(&frame_0_path, make_overlay_frame_file_path(&path, frame_index as FrameIndex))?;
            }
        }

        let frame_count = *frames.last().unwrap().index();
        let progress_style = ProgressStyle::with_template("{wide_bar} {pos:>6}/{len}").unwrap();
        // frames[0..20].par_iter().progress_with_style(progress_style).try_for_each(|frame| {
        frames.par_iter().progress_with_style(progress_style).try_for_each(|frame| {
            let actual_frame_index = (*frame.index() as i32 + frame_offset) as u32;
            log::debug!("{} -> {}", frame.index(), &actual_frame_index);
            let frame_image = self.draw_frame_overlay(frame).unwrap();
            frame_image.write_image_file(make_overlay_frame_file_path(&path, actual_frame_index))
        })?;

        log::info!("linking missing overlay frames");
        let frame_indices = frames.iter().map(|x| (*x.index() as i32 + frame_offset) as u32).collect();
        link_missing_frames(&path, &frame_indices)?;

        log::info!("overlay frames generation completed: {} frames", frame_count);
        Ok(())
    }

}