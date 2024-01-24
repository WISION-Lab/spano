use std::ops::DivAssign;

use conv::ValueInto;
use image::Pixel;
use imageproc::definitions::{Clamp, Image};
use itertools::multizip;
use ndarray::{array, concatenate, s, Array1, Array2, Array3, ArrayBase, Axis, Ix3, RawData};
use ndarray::{stack, Array};
use ndarray_interp::interp1d::{CubicSpline, Interp1DBuilder};
use ndarray_linalg::solve::Inverse;
use num_traits::AsPrimitive;
use rayon::iter::{IntoParallelIterator, IntoParallelRefMutIterator, ParallelIterator};
use rayon::slice::ParallelSliceMut;
use heapless::Vec as hVec;

use crate::transforms::{array3_to_image, ref_image_to_array3};

#[derive(Copy, Clone, Debug)]
pub enum TransformationType {
    Translational,
    Affine,
    Projective,
    Unknown,
}

impl TransformationType {
    pub fn num_params(&self) -> usize {
        match &self {
            TransformationType::Translational => 2,
            TransformationType::Affine => 6,
            TransformationType::Projective => 8,
            TransformationType::Unknown => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Mapping {
    pub mat: Array2<f32>,
    pub is_identity: bool,
    pub kind: TransformationType,
}

impl Mapping {
    /// Return the mapping that trasforms a point using a 3x3 matrix.
    pub fn from_matrix(mat: Array2<f32>, kind: TransformationType) -> Self {
        let is_identity = Array2::<f32>::eye(3).abs_diff_eq(&mat, 1e-8);
        Self {
            mat,
            is_identity,
            kind,
        }
    }

    /// Given a list of transform parameters, return a function that maps a
    /// source point to its destination. The type of mapping depends on the number of params (DoF).
    pub fn from_params(params: &[f32]) -> Self {
        let (full_params, kind) = match &params {
            // Translations
            [dx, dy] => (
                vec![1.0, 0.0, *dx, 0.0, 1.0, *dy, 0.0, 0.0, 1.0],
                TransformationType::Translational,
            ),

            // Affine Transforms
            [p1, p2, p3, p4, p5, p6] => (
                vec![*p1 + 1.0, *p3, *p5, *p2, *p4 + 1.0, *p6, 0.0, 0.0, 1.0],
                TransformationType::Affine,
            ),

            // Projective Transforms
            [p1, p2, p3, p4, p5, p6, p7, p8] => (
                vec![*p1 + 1.0, *p3, *p5, *p2, *p4 + 1.0, *p6, *p7, *p8, 1.0],
                TransformationType::Projective,
            ),
            _ => panic!(),
        };

        let mat = Array2::from_shape_vec((3, 3), full_params).unwrap();
        Self::from_matrix(mat, kind)
    }

    pub fn scale(x: f32, y: f32) -> Self {
        Self::from_params(&[x - 1.0, 0.0, 0.0, y - 1.0, 0.0, 0.0])
    }

    pub fn shift(x: f32, y: f32) -> Self {
        Self::from_params(&[x, y])
    }

    pub fn identity() -> Self {
        Self::from_params(&[0.0, 0.0])
    }

    #[inline]
    pub fn warp_points<T>(&self, points: &Array2<T>) -> Array2<f32>
    where
        T: AsPrimitive<f32> + Copy + 'static,
    {
        let points = points.mapv(|v| v.as_());

        if self.is_identity {
            return points;
        }

        let num_points = points.shape()[0];
        let points = concatenate![Axis(1), points, Array2::ones((num_points, 1))];

        let mut warped_points: Array2<f32> = self.mat.dot(&points.t());
        let d = warped_points.index_axis(Axis(0), 2).mapv(|v| v.max(1e-8));
        warped_points.div_assign(&d);

        warped_points.t().slice(s![.., ..2]).to_owned()
    }

    pub fn get_params(&self) -> Vec<f32> {
        let p = (&self.mat.clone() / self.mat[(2, 2)]).into_raw_vec();
        match &self.kind {
            TransformationType::Translational => vec![p[2], p[5]],
            TransformationType::Affine => vec![p[0] - 1.0, p[3], p[1], p[4] - 1.0, p[2], p[5]],
            TransformationType::Projective => {
                vec![p[0] - 1.0, p[3], p[1], p[4] - 1.0, p[2], p[5], p[6], p[7]]
            }
            _ => panic!("Transformation cannot be unknown!"),
        }
    }

    pub fn get_params_full(&self) -> Vec<f32> {
        let p = (&self.mat.clone() / self.mat[(2, 2)]).into_raw_vec();
        vec![p[0] - 1.0, p[3], p[1], p[4] - 1.0, p[2], p[5], p[6], p[7]]
    }

    pub fn inverse(&self) -> Self {
        Self {
            mat: self.mat.inv().expect("Cannot invert mapping"),
            is_identity: self.is_identity,
            kind: self.kind,
        }
    }

    pub fn transform(&self, lhs: Option<Self>, rhs: Option<Self>) -> Self {
        let (lhs_mat, lhs_id, lhs_kind) = lhs
            .map_or((Array2::eye(3), false, TransformationType::Unknown), |m| {
                (m.mat, m.is_identity, m.kind)
            });

        let (rhs_mat, rhs_id, rhs_kind) = rhs
            .map_or((Array2::eye(3), false, TransformationType::Unknown), |m| {
                (m.mat, m.is_identity, m.kind)
            });

        Mapping {
            mat: lhs_mat.dot(&self.mat).dot(&rhs_mat).to_owned(),
            is_identity: lhs_id & self.is_identity & rhs_id,
            kind: *[lhs_kind, self.kind, rhs_kind]
                .iter()
                .max_by_key(|k| k.num_params())
                .unwrap(),
        }
    }

    pub fn rescale(&self, scale: f32) -> Self {
        self.transform(
            Some(Mapping::scale(1.0 / scale, 1.0 / scale)),
            Some(Mapping::scale(scale, scale)),
        )
    }

    pub fn corners(&self, size: (usize, usize)) -> Array2<f32> {
        let (w, h) = size;
        let corners = array![[0, 0], [w, 0], [w, h], [0, h]];
        self.inverse().warp_points(&corners)
    }

    pub fn extent(&self, size: (usize, usize)) -> (Array1<f32>, Array1<f32>) {
        let corners = self.corners(size);
        let min_coords = corners.map_axis(Axis(0), |view| {
            view.iter().fold(f32::INFINITY, |a, b| a.min(*b))
        });
        let max_coords = corners.map_axis(Axis(0), |view| {
            view.iter().fold(-f32::INFINITY, |a, b| a.max(*b))
        });
        (min_coords, max_coords)
    }

    pub fn maximum_extent(maps: &[Self], size: (usize, usize)) -> (Array1<f32>, Self) {
        let (min_coords, max_coords): (Vec<Array1<f32>>, Vec<Array1<f32>>) =
            maps.iter().map(|m| m.extent(size)).unzip();

        let min_coords: Vec<_> = min_coords.iter().map(|arr| arr.view()).collect();
        let min_coords = stack(Axis(0), &min_coords[..])
            .unwrap()
            .map_axis(Axis(0), |view| {
                view.iter().fold(f32::INFINITY, |a, b| a.min(*b))
            });

        let max_coords: Vec<_> = max_coords.iter().map(|arr| arr.view()).collect();
        let max_coords = stack(Axis(0), &max_coords[..])
            .unwrap()
            .map_axis(Axis(0), |view| {
                view.iter().fold(-f32::INFINITY, |a, b| a.max(*b))
            });

        let extent = max_coords - &min_coords;
        let offset = Mapping::from_params(&min_coords.to_vec()[..]);
        (extent, offset)
    }

    pub fn interpolate_array(ts: Array1<f32>, maps: &Vec<Self>, query: Array1<f32>) -> Vec<Self> {
        let params = Array2::from_shape_vec(
            (maps.len(), 8),
            maps.iter().flat_map(|m| m.get_params_full()).collect(),
        )
        .unwrap();

        let interpolator = Interp1DBuilder::new(params)
            .x(ts)
            .strategy(CubicSpline::new())
            .build()
            .unwrap();

        let interp_params = interpolator.interp_array(&query).unwrap();
        interp_params
            .axis_iter(Axis(0))
            .map(|p| Self::from_params(&p.to_vec()))
            .collect()
    }

    pub fn interpolate_scalar(ts: Array1<f32>, maps: &Vec<Self>, query: f32) -> Self {
        Self::interpolate_array(ts, maps, array![query])
            .into_iter()
            .nth(0)
            .unwrap()
    }
}

pub fn warp_image<P>(
    mapping: &Mapping,
    data: &Image<P>,
    out_size: (usize, usize),
    background: Option<P>,
) -> Image<P>
where
    P: Pixel,
    <P as Pixel>::Subpixel:
        num_traits::Zero + Clone + Copy + ValueInto<f32> + Send + Sync + Clamp<f32>,
    f32: From<<P as Pixel>::Subpixel>,
{
    let arr = ref_image_to_array3(data);
    let background = background.map(|v| Array1::from_iter(v.channels().to_owned()));
    let (out, _) = warp_array3(mapping, &arr, out_size, background);
    array3_to_image(out)
}

pub fn warp_array3<S, T>(
    mapping: &Mapping,
    data: &ArrayBase<S, Ix3>,
    out_size: (usize, usize),
    background: Option<Array1<T>>,
) -> (Array3<T>, Array2<bool>)
where
    S: RawData<Elem = T> + ndarray::Data,
    T: num_traits::Zero + Clone + Copy + ValueInto<f32> + Send + Sync + Clamp<f32>,
    f32: From<T>,
{
    let (h, w) = out_size;
    let (_, _, c) = data.dim();
    let mut out = Array3::zeros((h, w, c));
    let mut valid = Array2::from_elem((h, w), false);
    warp_array3_into(mapping, data, &mut out, &mut valid, None, background, None);
    (out, valid)
}

/// Main workhorse for warping, use directly if output/points buffers can be 
/// reused or if something other than simple assignment is needed.
/// 
/// func:
///     Option of a function that describes what to do with sampled pixel.
///     It takes a mutable reference slice of the `out` buffer and a (possibly longer)
///     ref slice of the new sampled pixel.       
pub fn warp_array3_into<S, T>(
    mapping: &Mapping,
    data: &ArrayBase<S, Ix3>,
    out: &mut Array3<T>,
    valid: &mut Array2<bool>,
    points: Option<&Array2<usize>>,
    background: Option<Array1<T>>,
    func: Option<fn(&mut[T], &[T])>,
) where
    S: RawData<Elem = T> + ndarray::Data,
    T: num_traits::Zero + Clone + Copy + ValueInto<f32> + Send + Sync + Clamp<f32>,
    f32: From<T>,
{
    const MAX_CHANNELS: usize = 8;
    let (out_h, out_w, out_c) = out.dim();
    let (data_h, data_w, data_c) = data.dim();

    if out_c > MAX_CHANNELS || data_c > MAX_CHANNELS {
        panic!(
            "Maximum supported channel depth is {MAX_CHANNELS}, data \
            has {data_c} channels and output buffer has {out_c}."
        );
    }

    // Points is a Nx2 array of xy pairs
    let num_points = out_w * out_h;
    let points_: Array2<usize>;
    let points = if let Some(pts) = points {
        pts
    } else {
        points_ = Array::from_shape_fn(
            (num_points, 2),
            |(i, j)| {
                if j == 0 {
                    i % out_w
                } else {
                    i / out_w
                }
            },
        );
        &points_
    };

    // If no reduction function is present, simply assign to slice
    let func = func.unwrap_or(|dst, src| dst.iter_mut().zip(src).for_each(|(d, s)| *d = *s));

    // If a background is specified, use that, otherwise use zeros
    let (background, padding, has_bkg) = if let Some(bkg) = background {
        (bkg, 1.0, true)
    } else {
        (Array1::<T>::zeros(out_c), 0.0, false)
    };

    // Warp all points and determine indices of in-bound ones
    let warpd = mapping.warp_points(points);
    let in_range_x = |x: f32| -padding <= x && x <= (data_w as f32) - 1.0 + padding;
    let in_range_y = |y: f32| -padding <= y && y <= (data_h as f32) - 1.0 + padding;

    // Data sampler (enables smooth transition to bkg, i.e no jaggies)
    let bkg_slice = background
        .as_slice()
        .expect("Background should be contiguous in memory");
    let data_slice = data
        .as_slice()
        .expect("Data should be contiguous and HWC format");
    let get_pix_or_bkg = |x: f32, y: f32| {
        if x < 0f32 || x >= data_w as f32 || y < 0f32 || y >= data_h as f32 {
            bkg_slice
        } else {
            let offset = ((y as usize) * data_w + (x as usize)) * data_c;
            &data_slice[offset..offset + data_c]
        }
    };

    (
        out.as_slice_mut().unwrap().par_chunks_mut(out_c),
        valid.as_slice_mut().unwrap().par_iter_mut(),
        // warpd.column(0).as_slice().unwrap(),
        // warpd.column(1).as_slice().unwrap(),
        warpd.column(0).axis_iter(Axis(0)),
        warpd.column(1).axis_iter(Axis(0)),
    )
        .into_par_iter()
        .for_each(|(out_slice, valid_slice, x_, y_)| {
            let x = *x_.into_scalar();
            let y = *y_.into_scalar();

            if !in_range_x(x) || !in_range_y(y) {
                if has_bkg {
                    func(out_slice, bkg_slice);
                }
                *valid_slice = false;
                return;
            }

            // Actually do bilinear interpolation
            let left = x.floor();
            let right = left + 1f32;
            let top = y.floor();
            let bottom = top + 1f32;
            let right_weight = x - left;
            let left_weight = 1.0 - right_weight;
            let bottom_weight = y - top;
            let top_weight = 1.0 - bottom_weight;

            let (tl, tr, bl, br) = (
                get_pix_or_bkg(left, top),
                get_pix_or_bkg(right, top),
                get_pix_or_bkg(left, bottom),
                get_pix_or_bkg(right, bottom),
            );

            // Currently, the channel dimension cannot be known at compile time
            // even if it's usually either P::CHANNEL_COUNT, 3 or 1. Letting the compiler know
            // this info would be done via generic_const_exprs which are currenly unstable. 
            // Without this we can either:
            //      1) Collect all channels into a Vec and process that, which incurs a _lot_
            //         of allocs of small vectors (one per pixel), but allows for whole pixel operations.
            //      2) Process subpixels in a streaming manner with iterators. Avoids unneccesary
            //         allocs but constrains us to only subpixel ops (add, mul, etc). 
            // We choose to collect into a vector for greater flexibility, however we use a heapless 
            // vectors which saves us from the alloc at the cost of a constant and maximum channel depth. 
            // The alternative (subpix) was implemented in commit "[main 7ecb546] load photoncube".
            // See: https://github.com/rust-lang/rust/issues/76560
            let value: hVec<T, MAX_CHANNELS> = multizip((tl, tr, bl, br)).map(|(tl, tr, bl, br)| {
                T::clamp(
                    top_weight * left_weight * f32::from(*tl)
                        + top_weight * right_weight * f32::from(*tr)
                        + bottom_weight * left_weight * f32::from(*bl)
                        + bottom_weight * right_weight * f32::from(*br),
                )
            }).collect();

            func(out_slice, &value);
            *valid_slice = true;
        });
}

// ----------------------------------------------------------------------------
#[cfg(test)]
mod test_warps {
    use approx::assert_relative_eq;
    use ndarray::array;

    use crate::warps::{Mapping, TransformationType};

    #[test]
    fn test_warp_points() {
        let map = Mapping::from_matrix(
            array![
                [1.13411823, 4.38092511, 9.315785],
                [1.37351153, 5.27648111, 1.60252762],
                [7.76114426, 9.66312177, 2.61286966]
            ],
            TransformationType::Projective,
        );
        let point = array![[0, 0]];
        let warpd = map.warp_points(&point);
        assert_relative_eq!(warpd, array![[3.56534624, 0.61332092]]);
    }
}
