use std::{path::Path, sync::mpsc::Sender};

use crate::{
    panel_export::{ExportFileType, PanelExport},
    worldgen::{Step, WorldGenerator},
    ThreadMessage,
};

pub fn export_heightmap(
    // random number generator's seed to use
    seed: u64,
    // list of generator steps with their configuration and optional masks
    steps: &[Step],
    // size and number of files to export, file name pattern
    export_data: &PanelExport,
    // channel to send feedback messages to the main thread
    tx: Sender<ThreadMessage>,
    // minimum amount of progress to report (below this value, the global %age won't change)
    min_progress_step: f32,
) -> Result<(), String> {
    // The apron is context, not content: the tile still covers the same terrain,
    // the file just carries one extra ring of the neighbours' heights around it.
    let apron = if export_data.apron { 1 } else { 0 };
    let file_width = export_data.export_width as usize + 2 * apron;
    let file_height = export_data.export_height as usize + 2 * apron;
    let world_size = (
        (export_data.export_width * export_data.tiles_h) as usize,
        (export_data.export_height * export_data.tiles_v) as usize,
    );
    let mut wgen = WorldGenerator::new(seed, world_size);
    wgen.generate(steps, tx, min_progress_step);

    let (min, max) = wgen.get_min_max();
    let coef = if max - min > std::f32::EPSILON {
        1.0 / (max - min)
    } else {
        1.0
    };

    // Step by the tile's own size, less the shared edge. An apron implies that
    // shared edge, since the ring is what surrounds it.
    let overlap = usize::from(export_data.seamless || export_data.apron);
    let step_x = export_data.export_width as usize - overlap;
    let step_y = export_data.export_height as usize - overlap;

    for ty in 0..export_data.tiles_v as usize {
        for tx in 0..export_data.tiles_h as usize {
            // Signed: tile 0 starts one pixel before the field when aproned,
            // and the sampler clamps there.
            let offset_x = (tx * step_x) as isize - apron as isize;
            let offset_y = (ty * step_y) as isize - apron as isize;
            let path = format!(
                "{}_x{}_y{}.{}",
                export_data.file_path,
                tx,
                ty,
                export_data.file_type.to_string()
            );
            match export_data.file_type {
                ExportFileType::PNG => write_png(
                    file_width,
                    file_height,
                    offset_x,
                    offset_y,
                    &wgen,
                    world_size,
                    min,
                    coef,
                    &path,
                )?,
                ExportFileType::EXR => write_exr(
                    file_width,
                    file_height,
                    offset_x,
                    offset_y,
                    &wgen,
                    world_size,
                    min,
                    coef,
                    &path,
                )?,
            }
        }
    }
    Ok(())
}

/// Height at a signed field coordinate, clamped to the generated world.
///
/// Only the apron ever asks for a coordinate outside the field, and only at the
/// very edge of the world where there is no neighbour to describe. Clamping
/// repeats the border height, which is what a tile without an apron would have
/// done anyway; reading out of bounds would instead return 0.0 and cut a cliff
/// around the whole map.
fn sample_clamped(wgen: &WorldGenerator, x: isize, y: isize, size: (usize, usize)) -> f32 {
    let cx = x.clamp(0, size.0 as isize - 1) as usize;
    let cy = y.clamp(0, size.1 as isize - 1) as usize;
    wgen.combined_height(cx, cy)
}

fn write_png(
    file_width: usize,
    file_height: usize,
    offset_x: isize,
    offset_y: isize,
    wgen: &WorldGenerator,
    world_size: (usize, usize),
    min: f32,
    coef: f32,
    path: &str,
) -> Result<(), String> {
    let mut buf = vec![0u8; file_width * file_height * 2];
    for py in 0..file_height {
        for px in 0..file_width {
            let mut h = sample_clamped(
                wgen,
                px as isize + offset_x,
                py as isize + offset_y,
                world_size,
            );
            h = (h - min) * coef;
            let offset = (px + py * file_width) * 2;
            let pixel = (h * 65535.0) as u16;
            let upixel = pixel.to_ne_bytes();
            buf[offset] = upixel[0];
            buf[offset + 1] = upixel[1];
        }
    }
    image::save_buffer(
        &Path::new(&path),
        &buf,
        file_width as u32,
        file_height as u32,
        image::ColorType::L16,
    )
    .map_err(|e| format!("Error while saving {}: {}", &path, e))
}

fn write_exr(
    file_width: usize,
    file_height: usize,
    offset_x: isize,
    offset_y: isize,
    wgen: &WorldGenerator,
    world_size: (usize, usize),
    min: f32,
    coef: f32,
    path: &str,
) -> Result<(), String> {
    use exr::prelude::*;

    let channel = SpecificChannels::new(
        (ChannelDescription::named("Y", SampleType::F16),),
        |Vec2(px, py)| {
            let h = sample_clamped(
                wgen,
                px as isize + offset_x,
                py as isize + offset_y,
                world_size,
            );
            let h = f16::from_f32((h - min) * coef);
            (h,)
        },
    );

    Image::from_encoded_channels(
        (file_width, file_height),
        Encoding {
            compression: Compression::ZIP1,
            blocks: Blocks::ScanLines,
            line_order: LineOrder::Increasing,
        },
        channel,
    )
    .write()
    .to_file(path)
    .map_err(|e| format!("Error while saving {}: {}", &path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generators::MidPointConf;
    use crate::worldgen::StepType;

    /// Export a small world and hand back the tile pixel grids.
    fn export_tiles(apron: bool, tile_px: usize, tiles: usize) -> Vec<Vec<Vec<u16>>> {
        let dir = std::env::temp_dir().join(format!("wgen_apron_test_{apron}_{tile_px}_{tiles}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut export = PanelExport::default();
        export.export_width = tile_px as f32;
        export.export_height = tile_px as f32;
        export.tiles_h = tiles as f32;
        export.tiles_v = tiles as f32;
        export.seamless = true;
        export.apron = apron;
        export.file_path = format!("{}/t", dir.display());

        let steps = vec![Step {
            disabled: false,
            mask: None,
            typ: StepType::MidPoint(MidPointConf::default()),
        }];
        let (tx, rx) = std::sync::mpsc::channel();
        // Drain in the background; the generator blocks on a full channel.
        std::thread::spawn(move || while rx.recv().is_ok() {});
        export_heightmap(42, &steps, &export, tx, 0.1).unwrap();

        let expected = tile_px + if apron { 2 } else { 0 };
        (0..tiles)
            .map(|ty| {
                (0..tiles)
                    .map(|tx| {
                        let p = format!("{}/t_x{tx}_y{ty}.png", dir.display());
                        let img = image::open(&p).unwrap().to_luma16();
                        assert_eq!(
                            img.dimensions(),
                            (expected as u32, expected as u32),
                            "{p} has the wrong size"
                        );
                        img.pixels().map(|p| p.0[0]).collect()
                    })
                    .collect()
            })
            .collect()
    }

    /// The apron ring must be the neighbour's terrain, not a repeat of the
    /// tile's own edge — that is the whole reason it exists.
    #[test]
    fn apron_ring_holds_the_neighbours_heights() {
        let tile_px = 9; // 2^3 + 1
        let tiles = 3;
        let grids = export_tiles(true, tile_px, tiles);
        let w = tile_px + 2;
        let at = |g: &Vec<u16>, x: usize, y: usize| g[x + y * w];

        // Tile (0,0)'s right apron column is tile (1,0)'s first real column,
        // and its last real column is tile (1,0)'s left apron.
        let a = &grids[0][0];
        let b = &grids[0][1];
        for y in 0..w {
            assert_eq!(
                at(a, w - 1, y),
                at(b, 2, y),
                "right apron of (0,0) != column 1 of (1,0) at y={y}"
            );
            assert_eq!(
                at(a, w - 2, y),
                at(b, 1, y),
                "shared edge disagrees at y={y}"
            );
            assert_eq!(
                at(a, w - 3, y),
                at(b, 0, y),
                "left apron of (1,0) != last interior column of (0,0) at y={y}"
            );
        }
    }

    /// Without the apron the files keep their old size and overlap by exactly
    /// one shared row, so existing pipelines are untouched.
    #[test]
    fn seamless_without_apron_is_unchanged() {
        let tile_px = 9;
        let grids = export_tiles(false, tile_px, 2);
        let a = &grids[0][0];
        let b = &grids[0][1];
        for y in 0..tile_px {
            assert_eq!(
                a[(tile_px - 1) + y * tile_px],
                b[y * tile_px],
                "shared edge disagrees at y={y}"
            );
        }
    }
}
