use std::ffi::CString;
use std::ptr;

use ffmpeg_sys_next as ff;

struct SampledBg {
    reference_g: u8,
    target_cb: f64,
    target_cr: f64,
}

pub struct VideoPlayer {
    fmt_ctx:    *mut ff::AVFormatContext,
    codec_ctx:  *mut ff::AVCodecContext,
    sws_ctx:    *mut ff::SwsContext,
    frame:      *mut ff::AVFrame,
    frame_rgba: *mut ff::AVFrame,
    pkt:        *mut ff::AVPacket,
    stream_idx: i32,
    pub width:  u32,
    pub height: u32,
    rgba_buf:   Vec<u8>,
    done:       bool,
    reference_g: u8,
    target_cb: f64,
    target_cr: f64,
}

unsafe impl Send for VideoPlayer {}

impl VideoPlayer {
    pub fn open_fit(path: &str, max_size: u32) -> Option<Self> {
        unsafe {
            let path_c = CString::new(path).ok()?;
            let mut fmt_ctx: *mut ff::AVFormatContext = ptr::null_mut();
            if ff::avformat_open_input(&mut fmt_ctx, path_c.as_ptr(), ptr::null(), ptr::null_mut()) < 0 {
                return None;
            }
            ff::avformat_find_stream_info(fmt_ctx, ptr::null_mut());
            let nb = (*fmt_ctx).nb_streams as usize;
            let streams = std::slice::from_raw_parts((*fmt_ctx).streams, nb);
            let si = streams.iter().position(|&s| {
                (*(*s).codecpar).codec_type == ff::AVMediaType::AVMEDIA_TYPE_VIDEO
            })? as i32;
            let codecpar = (*streams[si as usize]).codecpar;
            let src_w = (*codecpar).width as u32;
            let src_h = (*codecpar).height as u32;
            let (out_w, out_h) = if src_w >= src_h {
                (max_size, (max_size as f64 * src_h as f64 / src_w as f64).round() as u32)
            } else {
                ((max_size as f64 * src_w as f64 / src_h as f64).round() as u32, max_size)
            };
            let out_w = out_w.max(1);
            let out_h = out_h.max(1);
            ff::avformat_close_input(&mut fmt_ctx);
            Self::open(path, out_w, out_h)
        }
    }

    fn decode_first_frame_and_sample(
        rgba_buf: &mut Vec<u8>,
        fmt_ctx: *mut ff::AVFormatContext,
        codec_ctx: *mut ff::AVCodecContext,
        sws_ctx: *mut ff::SwsContext,
        frame: *mut ff::AVFrame,
        frame_rgba: *mut ff::AVFrame,
        pkt: *mut ff::AVPacket,
        stream_idx: i32,
    ) -> SampledBg {
        unsafe {
            loop {
                let ret = ff::av_read_frame(fmt_ctx, pkt);
                if ret < 0 { break; }
                if (*pkt).stream_index != stream_idx {
                    ff::av_packet_unref(pkt);
                    continue;
                }
                let ret = ff::avcodec_send_packet(codec_ctx, pkt);
                ff::av_packet_unref(pkt);
                if ret < 0 { continue; }
                let ret = ff::avcodec_receive_frame(codec_ctx, frame);
                if ret == ff::AVERROR(ff::EAGAIN as i32) || ret == ff::AVERROR_EOF {
                    continue;
                }
                if ret < 0 { continue; }

                (*frame_rgba).data[0] = rgba_buf.as_mut_ptr();
                ff::sws_scale(
                    sws_ctx,
                    (*frame).data.as_ptr() as *const *const u8,
                    (*frame).linesize.as_ptr(),
                    0,
                    (*frame).height,
                    (*frame_rgba).data.as_mut_ptr(),
                    (*frame_rgba).linesize.as_ptr(),
                );
                ff::av_frame_unref(frame);

                let w = (*frame).width as u32;
                let h = (*frame).height as u32;
                let sampled = Self::sample_from_corners(rgba_buf, w, h);

                ff::av_seek_frame(fmt_ctx, stream_idx, 0, ff::AVSEEK_FLAG_BACKWARD as i32);
                ff::avcodec_flush_buffers(codec_ctx);

                return sampled;
            }
        }
        SampledBg { reference_g: 252, target_cb: 44.0, target_cr: 22.0 }
    }

    fn sample_from_corners(rgba: &[u8], w: u32, h: u32) -> SampledBg {
        let corners = [
            (0, 0),
            (w.saturating_sub(10), 0),
            (0, h.saturating_sub(10)),
            (w.saturating_sub(10), h.saturating_sub(10)),
        ];
        let mut total_g: u32 = 0;
        let mut total_cb: f64 = 0.0;
        let mut total_cr: f64 = 0.0;
        let mut count: u32 = 0;
        for &(cx, cy) in &corners {
            for dy in 0..10u32 {
                for dx in 0..10u32 {
                    let x = cx.saturating_add(dx);
                    let y = cy.saturating_add(dy);
                    if x < w && y < h {
                        let idx = ((y * w + x) * 4) as usize;
                        if idx + 3 < rgba.len() {
                            let r = rgba[idx] as u32;
                            let g = rgba[idx + 1] as u32;
                            let b = rgba[idx + 2] as u32;
                            if g > 50 && g > r && g > b {
                                total_g += g;
                                let rf = r as f64;
                                let gf = g as f64;
                                let bf = b as f64;
                                let cb = 128.0 + (-0.168736 * rf - 0.331264 * gf + 0.5 * bf);
                                let cr = 128.0 + (0.5 * rf - 0.418688 * gf - 0.081312 * bf);
                                total_cb += cb;
                                total_cr += cr;
                                count += 1;
                            }
                        }
                    }
                }
            }
        }
        if count > 0 {
            SampledBg {
                reference_g: (total_g / count) as u8,
                target_cb: total_cb / count as f64,
                target_cr: total_cr / count as f64,
            }
        } else {
            SampledBg { reference_g: 252, target_cb: 44.0, target_cr: 22.0 }
        }
    }

    fn soft_matte(rgba: &mut [u8], target_cb: f64, target_cr: f64) {
        let chroma_threshold: f64 = 40.0;

        for i in (0..rgba.len()).step_by(4) {
            let r = rgba[i] as f64;
            let g = rgba[i + 1] as f64;
            let b = rgba[i + 2] as f64;

            let cb = 128.0 + (-0.168736 * r - 0.331264 * g + 0.5 * b);
            let cr = 128.0 + (0.5 * r - 0.418688 * g - 0.081312 * b);

            let dcb = cb - target_cb;
            let dcr = cr - target_cr;
            let chroma_dist = (dcb * dcb + dcr * dcr).sqrt();

            // Use only chroma (Cb, Cr) distance — independent of brightness.
            // Pure green has very low Cb (around 43-45) and low Cr (around 22).
            // Natural colors have Cb in 80-150 range, Cr in 130-180 range.
            // This separates background from foreground reliably regardless of
            // brightness or alpha blending at edges.
            if chroma_dist < chroma_threshold {
                rgba[i] = 0;
                rgba[i + 1] = 0;
                rgba[i + 2] = 0;
                rgba[i + 3] = 0;
            } else {
                rgba[i + 3] = 255;
            }
        }
    }

    pub fn open(path: &str, out_w: u32, out_h: u32) -> Option<Self> {
        unsafe {
            let path_c = CString::new(path).ok()?;
            let mut fmt_ctx: *mut ff::AVFormatContext = ptr::null_mut();

            if ff::avformat_open_input(&mut fmt_ctx, path_c.as_ptr(), ptr::null(), ptr::null_mut()) < 0 {
                return None;
            }
            if ff::avformat_find_stream_info(fmt_ctx, ptr::null_mut()) < 0 {
                ff::avformat_close_input(&mut fmt_ctx);
                return None;
            }

            let nb = (*fmt_ctx).nb_streams as usize;
            let streams = std::slice::from_raw_parts((*fmt_ctx).streams, nb);
            let stream_idx = streams.iter().position(|&s| {
                (*(*s).codecpar).codec_type == ff::AVMediaType::AVMEDIA_TYPE_VIDEO
            })? as i32;

            let codecpar = (*streams[stream_idx as usize]).codecpar;
            let codec = ff::avcodec_find_decoder((*codecpar).codec_id);
            if codec.is_null() {
                ff::avformat_close_input(&mut fmt_ctx);
                return None;
            }

            let codec_ctx = ff::avcodec_alloc_context3(codec);
            if codec_ctx.is_null() {
                ff::avformat_close_input(&mut fmt_ctx);
                return None;
            }
            ff::avcodec_parameters_to_context(codec_ctx, codecpar);
            if ff::avcodec_open2(codec_ctx, codec, ptr::null_mut()) < 0 {
                ff::avcodec_free_context(&mut { codec_ctx });
                ff::avformat_close_input(&mut fmt_ctx);
                return None;
            }

            let src_w = (*codec_ctx).width;
            let src_h = (*codec_ctx).height;
            let src_fmt = (*codec_ctx).pix_fmt;

            let sws_ctx = ff::sws_getContext(
                src_w, src_h, src_fmt,
                out_w as i32, out_h as i32, ff::AVPixelFormat::AV_PIX_FMT_RGBA,
                2, ptr::null_mut(), ptr::null_mut(), ptr::null(),
            );
            if sws_ctx.is_null() {
                ff::avcodec_free_context(&mut { codec_ctx });
                ff::avformat_close_input(&mut fmt_ctx);
                return None;
            }

            let frame = ff::av_frame_alloc();
            let frame_rgba = ff::av_frame_alloc();
            let pkt = ff::av_packet_alloc();

            let rgba_size = (out_w * out_h * 4) as usize;
            let mut rgba_buf = vec![0u8; rgba_size];

            let linesize = (out_w * 4) as i32;
            (*frame_rgba).width = out_w as i32;
            (*frame_rgba).height = out_h as i32;
            (*frame_rgba).format = ff::AVPixelFormat::AV_PIX_FMT_RGBA as i32;
            (*frame_rgba).data[0] = rgba_buf.as_mut_ptr();
            (*frame_rgba).linesize[0] = linesize;

            let sampled_bg = Self::decode_first_frame_and_sample(&mut rgba_buf, fmt_ctx, codec_ctx, sws_ctx, frame, frame_rgba, pkt, stream_idx);

            Some(VideoPlayer {
                fmt_ctx, codec_ctx, sws_ctx, frame, frame_rgba, pkt,
                stream_idx,
                width: out_w, height: out_h,
                rgba_buf,
                done: false,
                reference_g: sampled_bg.reference_g,
                target_cb: sampled_bg.target_cb,
                target_cr: sampled_bg.target_cr,
            })
        }
    }

    pub fn next_frame(&mut self) -> Option<&[u8]> {
        unsafe {
            loop {
                if self.done {
                    self.seek_to_start();
                }

                let ret = ff::av_read_frame(self.fmt_ctx, self.pkt);
                if ret < 0 {
                    self.done = true;
                    continue;
                }

                if (*self.pkt).stream_index != self.stream_idx {
                    ff::av_packet_unref(self.pkt);
                    continue;
                }

                let ret = ff::avcodec_send_packet(self.codec_ctx, self.pkt);
                ff::av_packet_unref(self.pkt);
                if ret < 0 {
                    continue;
                }

                let ret = ff::avcodec_receive_frame(self.codec_ctx, self.frame);
                if ret == ff::AVERROR(ff::EAGAIN as i32) || ret == ff::AVERROR_EOF {
                    continue;
                }
                if ret < 0 {
                    continue;
                }

                (*self.frame_rgba).data[0] = self.rgba_buf.as_mut_ptr();
                ff::sws_scale(
                    self.sws_ctx,
                    (*self.frame).data.as_ptr() as *const *const u8,
                    (*self.frame).linesize.as_ptr(),
                    0,
                    (*self.frame).height,
                    (*self.frame_rgba).data.as_mut_ptr(),
                    (*self.frame_rgba).linesize.as_mut_ptr(),
                );
                ff::av_frame_unref(self.frame);

                Self::soft_matte(&mut self.rgba_buf, self.target_cb, self.target_cr);
                return Some(&self.rgba_buf);
            }
        }
    }

    fn seek_to_start(&mut self) {
        unsafe {
            ff::av_seek_frame(self.fmt_ctx, self.stream_idx, 0, ff::AVSEEK_FLAG_BACKWARD as i32);
            ff::avcodec_flush_buffers(self.codec_ctx);
            self.done = false;
        }
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        unsafe {
            if !self.pkt.is_null()       { ff::av_packet_free(&mut self.pkt); }
            if !self.frame.is_null()     { ff::av_frame_free(&mut self.frame); }
            if !self.frame_rgba.is_null(){ ff::av_frame_free(&mut self.frame_rgba); }
            if !self.sws_ctx.is_null()   { ff::sws_freeContext(self.sws_ctx); }
            if !self.codec_ctx.is_null() { ff::avcodec_free_context(&mut self.codec_ctx); }
            if !self.fmt_ctx.is_null()   { ff::avformat_close_input(&mut self.fmt_ctx); }
        }
    }
}
