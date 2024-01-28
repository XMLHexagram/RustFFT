use std::sync::Arc;

use num_complex::Complex;
use num_traits::Zero;

use crate::algorithm::butterflies::{Butterfly1, Butterfly16, Butterfly2, Butterfly4, Butterfly8};
use crate::array_utils::{self, bitreversed_transpose};
use crate::common::{fft_error_inplace, fft_error_outofplace};
use crate::{common::FftNum, twiddles, FftDirection};
use crate::{Direction, Fft, Length};

/// FFT algorithm optimized for power-of-two sizes
///
/// ~~~
/// // Computes a forward FFT of size 4096
/// use rustfft::algorithm::Radix4;
/// use rustfft::{Fft, FftDirection};
/// use rustfft::num_complex::Complex;
///
/// let mut buffer = vec![Complex{ re: 0.0f32, im: 0.0f32 }; 4096];
///
/// let fft = Radix4::new(4096, FftDirection::Forward);
/// fft.process(&mut buffer);
/// ~~~

pub struct Radix4<T> {
    twiddles: Box<[Complex<T>]>,

    base_fft: Arc<dyn Fft<T>>,
    base_len: usize,

    len: usize,
    direction: FftDirection,
}

impl<T: FftNum> Radix4<T> {
    /// Preallocates necessary arrays and precomputes necessary data to efficiently compute the power-of-two FFT
    pub fn new(len: usize, direction: FftDirection) -> Self {
        assert!(
            len.is_power_of_two(),
            "Radix4 algorithm requires a power-of-two input size. Got {}",
            len
        );

        // figure out which base length we're going to use
        let num_bits = len.trailing_zeros();
        let (base_len, base_fft) = match num_bits {
            0 => (len, Arc::new(Butterfly1::new(direction)) as Arc<dyn Fft<T>>),
            1 => (len, Arc::new(Butterfly2::new(direction)) as Arc<dyn Fft<T>>),
            2 => (len, Arc::new(Butterfly4::new(direction)) as Arc<dyn Fft<T>>),
            _ => {
                if num_bits % 2 == 1 {
                    (8, Arc::new(Butterfly8::new(direction)) as Arc<dyn Fft<T>>)
                } else {
                    (16, Arc::new(Butterfly16::new(direction)) as Arc<dyn Fft<T>>)
                }
            }
        };

        // precompute the twiddle factors this algorithm will use.
        // we're doing the same precomputation of twiddle factors as the mixed radix algorithm where width=4 and height=len/4
        // but mixed radix only does one step and then calls itself recusrively, and this algorithm does every layer all the way down
        // so we're going to pack all the "layers" of twiddle factors into a single array, starting with the bottom layer and going up
        const ROW_COUNT: usize = 4;
        let mut cross_fft_len = base_len * ROW_COUNT;
        let mut twiddle_factors = Vec::with_capacity(len * 2);
        while cross_fft_len <= len {
            let num_columns = cross_fft_len / ROW_COUNT;

            for i in 0..num_columns {
                for k in 1..ROW_COUNT {
                    let twiddle = twiddles::compute_twiddle(i * k, cross_fft_len, direction);
                    twiddle_factors.push(twiddle);
                }
            }
            cross_fft_len *= ROW_COUNT;
        }

        Self {
            twiddles: twiddle_factors.into_boxed_slice(),

            base_fft,
            base_len,

            len,
            direction,
        }
    }

    fn perform_fft_out_of_place(
        &self,
        input: &[Complex<T>],
        output: &mut [Complex<T>],
        _scratch: &mut [Complex<T>],
    ) {
        // copy the data into the output vector
        if self.len() == self.base_len {
            output.copy_from_slice(input);
        } else {
            bitreversed_transpose::<Complex<T>, 4>(self.base_len, input, output);
        }

        // Base-level FFTs
        self.base_fft.process_with_scratch(output, &mut []);

        // cross-FFTs
        const ROW_COUNT: usize = 4;
        let mut cross_fft_len = self.base_len * ROW_COUNT;
        let mut layer_twiddles: &[Complex<T>] = &self.twiddles;

        while cross_fft_len <= input.len() {
            let num_rows = input.len() / cross_fft_len;
            let num_columns = cross_fft_len / ROW_COUNT;

            for i in 0..num_rows {
                unsafe {
                    butterfly_4(
                        &mut output[i * cross_fft_len..],
                        layer_twiddles,
                        num_columns,
                        self.direction,
                    )
                }
            }

            // skip past all the twiddle factors used in this layer
            let twiddle_offset = num_columns * (ROW_COUNT - 1);
            layer_twiddles = &layer_twiddles[twiddle_offset..];

            cross_fft_len *= ROW_COUNT;
        }
    }
}
boilerplate_fft_oop!(Radix4, |this: &Radix4<_>| this.len);

unsafe fn butterfly_4<T: FftNum>(
    data: &mut [Complex<T>],
    twiddles: &[Complex<T>],
    num_ffts: usize,
    direction: FftDirection,
) {
    let butterfly4 = Butterfly4::new(direction);

    let mut idx = 0usize;
    let mut tw_idx = 0usize;
    let mut scratch = [Zero::zero(); 4];
    for _ in 0..num_ffts {
        scratch[0] = *data.get_unchecked(idx);
        scratch[1] = *data.get_unchecked(idx + 1 * num_ffts) * twiddles[tw_idx];
        scratch[2] = *data.get_unchecked(idx + 2 * num_ffts) * twiddles[tw_idx + 1];
        scratch[3] = *data.get_unchecked(idx + 3 * num_ffts) * twiddles[tw_idx + 2];

        butterfly4.perform_fft_butterfly(&mut scratch);

        *data.get_unchecked_mut(idx) = scratch[0];
        *data.get_unchecked_mut(idx + 1 * num_ffts) = scratch[1];
        *data.get_unchecked_mut(idx + 2 * num_ffts) = scratch[2];
        *data.get_unchecked_mut(idx + 3 * num_ffts) = scratch[3];

        tw_idx += 3;
        idx += 1;
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::test_utils::check_fft_algorithm;

    #[test]
    fn test_radix4() {
        for pow in 1..12 {
            let len = 1 << pow;
            test_radix4_with_length(len, FftDirection::Forward);
            //test_radix4_with_length(len, FftDirection::Inverse);
        }
    }

    fn test_radix4_with_length(len: usize, direction: FftDirection) {
        let fft = Radix4::new(len, direction);

        check_fft_algorithm::<f32>(&fft, len, direction);
    }
}
