// pub use std::path::Path;
// use image::io::Reader as ImageReader;


// /// Given an Option of a path, try to load the image or npy file at that path
// /// and return it as a Result of an Option of array. Bubble up any io errors.  
// /// Currently only supports RGB8-type images.
// pub fn try_load_img(path_str: String) -> Result<Array3<u8>> {
//     let path = Path::new(&path_str);

//     if !path.exists() {
//         // This should probably be a specific IO error?
//         Err(anyhow!("File not found at {}!", path_str))
//     } else {
//         let arr = ImageReader::open(path_str)?
//             .decode()?
//             .into_luma8()
//             .into_ndarray2()
//             .mapv(|v| v != 0);
//         Ok(Some(arr))
//     }
// }

// /// Load either a 2D NPY file or an intensity-only image file as an array of booleans.
// /// Note: For the image, any pure white pixels are false, all others are true.
// ///       This is contrary to what you might expect but enables us to load in the
// ///       colorSPAD's cfa array and have a mask representing the colored pixels.
// pub fn try_load_mask(path: Option<String>) -> Result<Option<Array2<bool>>> {
//     if path.is_none() {
//         return Ok(None);
//     }

//     let path_str = path.unwrap();
//     let path = Path::new(&path_str);
//     let ext = path.extension().unwrap().to_ascii_lowercase();

//     if !path.exists() {
//         // This should probably be a specific IO error?
//         Err(anyhow!("File not found at {}!", path_str))
//     } else if ext == "npy" || ext == "npz" {
//         let arr: Array2<bool> = read_npy(path_str)?;
//         Ok(Some(arr))
//     } else {
//         let arr = ImageReader::open(path_str)?
//             .decode()?
//             .into_luma8()
//             .into_ndarray2()
//             .mapv(|v| v != 0);
//         Ok(Some(arr))
//     }
// }
