use std::{ops::DivAssign, str::FromStr};

use anyhow::Result;
use conv::ValueInto;
use heapless::Vec as hVec;
use image::Pixel;
use imageproc::definitions::{Clamp, Image};
use itertools::{chain, multizip};
use ndarray::{
    array, concatenate, s, stack, Array, Array1, Array2, Array3, ArrayBase, Axis, Ix3, RawData,
};
use ndarray_interp::interp1d::{CubicSpline, Interp1DBuilder, Linear};
use ndarray_linalg::solve::Inverse;
use num_traits::AsPrimitive;
use numpy::{PyArray1, PyArray2, PyArray3, ToPyArray};
use photoncube2video::transforms::{array3_to_image, ref_image_to_array3};
use pyo3::{prelude::*, types::PyType};
use rayon::{
    iter::{IntoParallelIterator, IntoParallelRefMutIterator, ParallelIterator},
    slice::ParallelSliceMut,
};
use strum_macros::{Display, EnumString};

// Note: We cannot use #[pyclass] her as we're stuck in pyo3@0.15.2 to support py36, so
// we use `EnumString` to convert strings into their enum values.
// TODO: Use pyclass and remove strum dependency when we drop py36 support.
#[derive(Copy, Clone, Debug, EnumString, Display, PartialEq)]
pub enum TransformationType {
    Identity,
    Translational,
    Affine,
    Projective,
    Unknown,
}

impl TransformationType {
    pub fn num_params(&self) -> usize {
        match &self {
            TransformationType::Identity => 0,
            TransformationType::Translational => 2,
            TransformationType::Affine => 6,
            TransformationType::Projective => 8,
            TransformationType::Unknown => 0,
        }
    }
}

#[pyclass]
#[derive(Debug, Clone)]
pub struct Mapping {
    pub mat: Array2<f32>,
    pub kind: TransformationType,
}

// Note: Methods in this `impl` block are _not_ exposed to python
impl Mapping {
    pub fn from_matrix(mat: Array2<f32>, kind: TransformationType) -> Self {
        Self { mat, kind }
    }

    pub fn warp_points<T>(&self, points: &Array2<T>) -> Array2<f32>
    where
        T: AsPrimitive<f32> + Copy + 'static,
    {
        let points = points.mapv(|v| v.as_());

        if self.kind == TransformationType::Identity {
            return points;
        }

        let num_points = points.shape()[0];
        let points = concatenate![Axis(1), points, Array2::ones((num_points, 1))];

        let mut warped_points: Array2<f32> = self.mat.dot(&points.t());
        let d = warped_points.index_axis(Axis(0), 2).mapv(|v| v.max(1e-8));
        warped_points.div_assign(&d);

        warped_points.t().slice(s![.., ..2]).to_owned()
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

    pub fn maximum_extent(maps: &[Self], sizes: &[(usize, usize)]) -> (Array1<f32>, Self) {
        // We detect which is longer and cycle the other one.
        let (min_coords, max_coords): (Vec<_>, Vec<_>) = if maps.len() >= sizes.len() {
            maps.iter()
                .zip(sizes.iter().cycle())
                .map(|(m, s)| m.extent(*s))
                .unzip()
        } else {
            sizes
                .iter()
                .zip(maps.iter().cycle())
                .map(|(s, m)| m.extent(*s))
                .unzip()
        };

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
        let offset = Mapping::from_params(min_coords.to_vec());
        (extent, offset)
    }

    pub fn warp_image<P>(
        &self,
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
        let (out, _) = self.warp_array3(&arr, out_size, background);
        array3_to_image(out)
    }

    pub fn warp_array3<S, T>(
        &self,
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
        self.warp_array3_into(data, &mut out, &mut valid, None, background, None);
        (out, valid)
    }

    /// Main workhorse for warping, use directly if output/points buffers can be
    /// reused or if something other than simple assignment is needed.
    ///
    /// func:
    ///     Option of a function that describes what to do with sampled pixel.
    ///     It takes a mutable reference slice of the `out` buffer and a (possibly longer)
    ///     ref slice of the new sampled pixel.    
    #[allow(clippy::type_complexity)]
    pub fn warp_array3_into<S, T>(
        &self,
        data: &ArrayBase<S, Ix3>,
        out: &mut Array3<T>,
        valid: &mut Array2<bool>,
        points: Option<&Array2<usize>>,
        background: Option<Array1<T>>,
        func: Option<fn(&mut [T], &[T])>,
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
        let points =
            if let Some(pts) = points {
                pts
            } else {
                points_ = Array::from_shape_fn((num_points, 2), |(i, j)| {
                    if j == 0 {
                        i % out_w
                    } else {
                        i / out_w
                    }
                });
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
        let warpd = self.warp_points(points);
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
                let value: hVec<T, MAX_CHANNELS> = multizip((tl, tr, bl, br))
                    .map(|(tl, tr, bl, br)| {
                        T::clamp(
                            top_weight * left_weight * f32::from(*tl)
                                + top_weight * right_weight * f32::from(*tr)
                                + bottom_weight * left_weight * f32::from(*bl)
                                + bottom_weight * right_weight * f32::from(*br),
                        )
                    })
                    .collect();

                func(out_slice, &value);
                *valid_slice = true;
            });
    }
}

// Note: Methods in this `impl` block are exposed to python
#[pymethods]
impl Mapping {
    /// Return a Mapping object based on it's 3x3 matrix.
    #[classmethod]
    #[pyo3(
        name = "from_matrix",
        text_signature = "(cls, mat: np.ndarray, kind: str) -> Self"
    )]
    pub fn from_matrix_py(_: &PyType, mat: &PyArray2<f32>, kind: &str) -> Result<Self> {
        Ok(Self::from_matrix(
            mat.to_owned_array(),
            TransformationType::from_str(kind)?,
        ))
    }

    /// Given a list of transform parameters, return the Mapping that would transform a
    /// source point to its destination. The type of mapping depends on the number of params (DoF).
    #[staticmethod]
    #[pyo3(text_signature = "(cls, params: List[float]) -> Self")]
    pub fn from_params(params: Vec<f32>) -> Self {
        let (full_params, kind) = match &params[..] {
            // Identity
            [] => (Array2::eye(3).into_raw_vec(), TransformationType::Identity),

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

    /// Return a purely scaling (affine) Mapping.
    #[staticmethod]
    #[pyo3(text_signature = "(cls, x: float, y: float) -> Self")]
    pub fn scale(x: f32, y: f32) -> Self {
        Self::from_params(vec![x - 1.0, 0.0, 0.0, y - 1.0, 0.0, 0.0])
    }

    /// Return a purely translational Mapping.
    #[staticmethod]
    #[pyo3(text_signature = "(cls, x: float, y: float) -> Self")]
    pub fn shift(x: f32, y: f32) -> Self {
        Self::from_params(vec![x, y])
    }

    /// Return an identity Mapping.
    #[staticmethod]
    #[pyo3(text_signature = "(cls) -> Self")]
    pub fn identity() -> Self {
        Self::from_params(vec![])
    }

    /// Get maximum extent of a collection of warps and theirs sizes.
    /// Sizes are expected to be (x, y) pairs, _not_ (h, w)/(y, x). Similarly extent will be (x, y).
    /// Maps and Sizes might be different lengths:
    ///     - Maybe all warps operate on a single size
    ///     - If warps are the same, this is just max size
    /// Returns an extent (max width, max height) and offset warp.
    #[staticmethod]
    #[pyo3(
        name = "maximum_extent",
        text_signature = "(maps: List[Self], sizes: List[(int, int)]) -> (np.ndarray, Self)"
    )]
    pub fn maximum_extent_py<'py>(
        py: Python<'py>,
        maps: Vec<Self>,
        sizes: Vec<(usize, usize)>,
    ) -> (&PyArray1<f32>, Self) {
        let (extent, offset) = Self::maximum_extent(&maps, &sizes);
        (extent.to_pyarray(py), offset)
    }

    /// Interpolate a list of Mappings and query a single point.
    /// Ex: Mapping.interpolate_scalar(
    ///         [0, 1],
    ///         [Mapping.identity(), Mapping.shift(10, 20)],
    ///         0.5
    ///     ) == Mapping.shift(5, 10)
    /// See `interpolate_array` for more.
    #[staticmethod]
    #[pyo3(text_signature = "(ts: List[float], maps: List[Self], query: float) -> Self:")]
    pub fn interpolate_scalar(ts: Vec<f32>, maps: Vec<Self>, query: f32) -> Self {
        Self::interpolate_array(ts, maps, vec![query])
            .into_iter()
            .nth(0)
            .unwrap()
    }

    /// Interpolate a list of Mappings and query multiple points.
    /// This defaults to performing a cubic spline iterpolation of the warp params, and
    /// falls back to linear interpolation if not enough data points are known (<2).
    #[staticmethod]
    #[pyo3(
        text_signature = "(ts: List[float], maps: List[Self], query: List[float]) -> List[Self]:"
    )]
    pub fn interpolate_array(ts: Vec<f32>, maps: Vec<Self>, query: Vec<f32>) -> Vec<Self> {
        let params = Array2::from_shape_vec(
            (maps.len(), 8),
            maps.iter().flat_map(|m| m.get_params_full()).collect(),
        )
        .unwrap();

        let interp_params = if maps.len() > 2 {
            let interpolator = Interp1DBuilder::new(params)
                .x(Array1::from_vec(ts))
                .strategy(CubicSpline::new())
                .build()
                .unwrap();
            interpolator.interp_array(&Array1::from_vec(query)).unwrap()
        } else {
            let interpolator = Interp1DBuilder::new(params)
                .x(Array1::from_vec(ts))
                .strategy(Linear::new())
                .build()
                .unwrap();
            interpolator.interp_array(&Array1::from_vec(query)).unwrap()
        };

        interp_params
            .axis_iter(Axis(0))
            .map(|p| Self::from_params(p.to_vec()))
            .collect()
    }

    /// Compose/accumulate all pairwise mappings together.
    #[staticmethod]
    #[pyo3(text_signature = "(mappings: List[Self]) -> List[Self]")]
    pub fn accumulate(mappings: Vec<Self>) -> Vec<Self> {
        // Add in an identity warp to the start to have one warp per frame
        // TODO: maybe impl Copy to minimize the clones here...
        // TODO: Can we avoid the above collect and cumulatively compose in parallel?
        chain([Mapping::identity()], mappings)
            .scan(Mapping::identity(), |acc, x| {
                *acc = acc.transform(None, Some(x.clone()));
                Some(acc.clone())
            })
            .collect()
    }

    /// Apply wrt correction such that the wrt warp becomes the identity.
    #[staticmethod]
    #[pyo3(text_signature = "(mappings: List[Self], wrt_map: Self) -> List[Self]")]
    pub fn with_respect_to(mappings: Vec<Self>, wrt_map: Self) -> Vec<Self> {
        mappings
            .iter()
            .map(|m| m.transform(Some(wrt_map.inverse()), None))
            .collect()
    }

    /// Apply wrt correction such that the interpolated warp at the
    /// normalized [0, 1] wrt_idx becomes the identity.
    #[staticmethod]
    #[pyo3(text_signature = "(mappings: List[Self], wrt_idx: float) -> List[Self]")]
    pub fn with_respect_to_idx(mappings: Vec<Self>, wrt_idx: f32) -> Vec<Self> {
        let wrt_map = Mapping::interpolate_scalar(
            Array::linspace(0.0, 1.0, mappings.len()).to_vec(),
            mappings.to_owned(),
            wrt_idx,
        );
        Self::with_respect_to(mappings, wrt_map)
    }

    /// Compose/accumulate all pairwise mappings together and apply wrt_idx correction
    /// such that the warp of the frame at the normalized [0, 1] wrt index is the identity.
    /// This effectively accumulates the warps, interpolates them to find the
    /// wrp mapping and then uses `with_respect_to_idx` to undo wrt mapping.
    #[staticmethod]
    #[pyo3(text_signature = "(mappings: List[Self], wrt_idx: float) -> List[Self]")]
    pub fn accumulate_wrt_idx(mappings: Vec<Self>, wrt_idx: f32) -> Vec<Self> {
        let mappings = Self::accumulate(mappings);
        let wrt_map = Mapping::interpolate_scalar(
            Array::linspace(0.0, 1.0, mappings.len()).to_vec(),
            mappings.to_owned(),
            wrt_idx,
        );
        Self::with_respect_to(mappings, wrt_map)
    }

    /// Get minimum number of parameters that describe the Mapping.
    #[pyo3(text_signature = "(self) -> List[float]")]
    pub fn get_params(&self) -> Vec<f32> {
        let p = (&self.mat.clone() / self.mat[(2, 2)]).into_raw_vec();
        match &self.kind {
            TransformationType::Identity => vec![],
            TransformationType::Translational => vec![p[2], p[5]],
            TransformationType::Affine => vec![p[0] - 1.0, p[3], p[1], p[4] - 1.0, p[2], p[5]],
            TransformationType::Projective => {
                vec![p[0] - 1.0, p[3], p[1], p[4] - 1.0, p[2], p[5], p[6], p[7]]
            }
            TransformationType::Unknown => panic!("Transformation cannot be unknown!"),
        }
    }

    /// Get all parameters of the Mapping (overparameterized for everything but projective warp).
    #[pyo3(text_signature = "(self) -> List[float]")]
    pub fn get_params_full(&self) -> Vec<f32> {
        let p = (&self.mat.clone() / self.mat[(2, 2)]).into_raw_vec();
        vec![p[0] - 1.0, p[3], p[1], p[4] - 1.0, p[2], p[5], p[6], p[7]]
    }

    /// Invert the mapping by creating new mapping with inverse matrix.
    #[pyo3(text_signature = "(self) -> Self")]
    pub fn inverse(&self) -> Self {
        Self {
            mat: self.mat.inv().expect("Cannot invert mapping"),
            kind: self.kind,
        }
    }

    /// Compose with other mappings from left or right. Useful for scaling, offsetting, etc...
    /// Resulting mapping will have be cast to the most general mapping kind of all inputs.
    #[pyo3(text_signature = "(self, *, lhs: Optional[Self], rhs: Optional[Self]) -> Self")]
    pub fn transform(&self, lhs: Option<Self>, rhs: Option<Self>) -> Self {
        let (lhs_mat, lhs_kind) = lhs.map_or((Array2::eye(3), TransformationType::Unknown), |m| {
            (m.mat, m.kind)
        });

        let (rhs_mat, rhs_kind) = rhs.map_or((Array2::eye(3), TransformationType::Unknown), |m| {
            (m.mat, m.kind)
        });

        Mapping {
            mat: lhs_mat.dot(&self.mat).dot(&rhs_mat).to_owned(),
            kind: *[lhs_kind, self.kind, rhs_kind]
                .iter()
                .max_by_key(|k| k.num_params())
                .unwrap(),
        }
    }

    /// Rescale mapping and keep it's kind intact. This enables a mapping to work
    /// for a rescaled image (since the pixel coordinates get changed too).
    #[pyo3(text_signature = "(self, scale: float) -> Self")]
    pub fn rescale(&self, scale: f32) -> Self {
        let mut map = self.transform(
            Some(Mapping::scale(1.0 / scale, 1.0 / scale)),
            Some(Mapping::scale(scale, scale)),
        );
        map.kind = self.kind;
        map
    }

    /// Warp a set of Nx2 points using the mapping.
    #[pyo3(
        name = "warp_points",
        text_signature = "(self, points: np.ndarray) -> np.ndarray"
    )]
    pub fn warp_points_py<'py>(
        &'py self,
        py: Python<'py>,
        points: &PyArray2<f32>,
    ) -> &PyArray2<f32> {
        self.warp_points(&points.to_owned_array()).to_pyarray(py)
    }

    /// Get location of corners of an image of shape `size` once warped with `self`.
    #[pyo3(
        name = "corners",
        text_signature = "(self, size: (int, int)) -> np.ndarray"
    )]
    pub fn corners_py<'py>(&'py self, py: Python<'py>, size: (usize, usize)) -> &PyArray2<f32> {
        self.corners(size).to_pyarray(py)
    }

    /// Equivalent to getting minimum and maximum x/y coordinates of `corners`.
    /// Returns (min x, min y), (max x, max y)
    #[pyo3(
        name = "extent",
        text_signature = "(self, size: (int, int)) -> (np.ndarray, np.ndarray)"
    )]
    pub fn extent_py<'py>(
        &'py self,
        py: Python<'py>,
        size: (usize, usize),
    ) -> (&PyArray1<f32>, &PyArray1<f32>) {
        let (min, max) = self.extent(size);
        (min.to_pyarray(py), max.to_pyarray(py))
    }

    /// Warp array using mapping into a new buffer of shape `out_size`.
    /// This returns the new buffer along with a mask of which pixelks were warped.
    #[pyo3(
        name = "warp_array",
        text_signature = "(self, data: np.ndarray, out_size: (int, int), \
        background: Optional[List[float]]) -> (np.ndarray, np.ndarray)"
    )]
    pub fn warp_array3_py<'py>(
        &'py self,
        py: Python<'py>,
        data: &PyArray3<f32>,
        out_size: (usize, usize),
        background: Option<Vec<f32>>,
    ) -> (&PyArray3<f32>, &PyArray2<bool>) {
        let (out, valid) = self.warp_array3(
            unsafe { &data.as_array() },
            out_size,
            background.map(|v| Array1::from_vec(v)),
        );
        (out.to_pyarray(py), valid.to_pyarray(py))
    }

    #[getter(mat)]
    pub fn mat_getter<'py>(&'py self, py: Python<'py>) -> Result<Py<PyAny>> {
        // See: https://github.com/PyO3/rust-numpy/issues/408
        let py_arr = self.mat.to_pyarray(py).to_owned().into_py(py);
        py_arr
            .getattr(py, "setflags")?
            .call1(py, (false, None::<bool>, None::<bool>))?;
        Ok(py_arr)
    }

    #[setter(mat)]
    pub fn mat_setter(&mut self, arr: &PyArray2<f32>) -> Result<()> {
        self.mat = arr.to_owned_array();
        Ok(())
    }

    #[getter(kind)]
    pub fn kind_getter(&self) -> String {
        self.kind.to_string()
    }

    #[setter(kind)]
    pub fn kind_setter(&mut self, kind: &str) -> Result<()> {
        self.kind = TransformationType::from_str(kind)?;
        Ok(())
    }

    pub fn __str__(&self) -> Result<String> {
        Ok(format!(
            "Mapping(mat={:6.4}, kind=\"{}\")",
            self.mat, self.kind
        ))
    }
}

// ----------------------------------------------------------------------------
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
