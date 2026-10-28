// LMA_AUDIO XMA decoding through FFmpeg's xma2 decoder (#199, feature `xma-ffmpeg`). A shim, so
// the Rust side never depends on libavcodec's struct layouts: one decoder per XMA context, one
// 2048-byte packet in, interleaved 16-bit samples out.

#include <libavcodec/avcodec.h>
#include <libavutil/channel_layout.h>
#include <libavutil/mem.h>
#include <string.h>

typedef struct {
    AVCodecContext *c;
    AVPacket *p;
    AVFrame *f;
} lma_xma;

lma_xma *lma_xma_open(int channels, int rate) {
    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_XMA2);
    if (!codec) return NULL;
    lma_xma *x = av_mallocz(sizeof *x);
    if (!x) return NULL;
    x->c = avcodec_alloc_context3(codec);
    x->p = av_packet_alloc();
    x->f = av_frame_alloc();
    if (!x->c || !x->p || !x->f) goto fail;
    x->c->sample_rate = rate;
    av_channel_layout_default(&x->c->ch_layout, channels);
    // XMA2WAVEFORMATEX (34 bytes, little-endian): one stream of `channels`
    x->c->extradata = av_mallocz(34 + AV_INPUT_BUFFER_PADDING_SIZE);
    if (!x->c->extradata) goto fail;
    x->c->extradata_size = 34;
    x->c->extradata[0] = 1;
    x->c->extradata[2] = channels == 2 ? 3 : 4; // channel mask: FL|FR, or FC
    if (avcodec_open2(x->c, codec, NULL) < 0) goto fail;
    return x;
fail:
    avcodec_free_context(&x->c);
    av_packet_free(&x->p);
    av_frame_free(&x->f);
    av_free(x);
    return NULL;
}

static short clamp16(float v) {
    v *= 32767.0f;
    return v > 32767.0f ? 32767 : v < -32768.0f ? -32768 : (short)v;
}

// Decodes one packet; writes up to `max` interleaved samples to `out`. Returns how many were
// written, or a negative FFmpeg error.
int lma_xma_decode(lma_xma *x, const unsigned char *pkt, int len, short *out, int max) {
    if (av_new_packet(x->p, len) < 0) return -1;
    memcpy(x->p->data, pkt, len);
    int r = avcodec_send_packet(x->c, x->p);
    av_packet_unref(x->p);
    if (r < 0 && r != AVERROR(EAGAIN)) return r;
    int n = 0;
    while (avcodec_receive_frame(x->c, x->f) >= 0) {
        int ch = x->f->ch_layout.nb_channels;
        for (int i = 0; i < x->f->nb_samples && n + ch <= max; i++) {
            for (int c = 0; c < ch; c++) {
                float v = x->f->format == AV_SAMPLE_FMT_FLTP ? ((const float *)x->f->extended_data[c])[i]
                                                             : ((const float *)x->f->data[0])[i * ch + c];
                out[n++] = clamp16(v);
            }
        }
        av_frame_unref(x->f);
    }
    return n;
}

// End of the stream: whatever the decoder still holds (it keeps one packet's frames back until
// the next packet), then ready for a fresh stream. Returns samples written, as `lma_xma_decode`.
int lma_xma_drain(lma_xma *x, short *out, int max) {
    int n = 0;
    if (avcodec_send_packet(x->c, NULL) >= 0) {
        while (avcodec_receive_frame(x->c, x->f) >= 0) {
            int ch = x->f->ch_layout.nb_channels;
            for (int i = 0; i < x->f->nb_samples && n + ch <= max; i++) {
                for (int c = 0; c < ch; c++) {
                    float v = x->f->format == AV_SAMPLE_FMT_FLTP ? ((const float *)x->f->extended_data[c])[i]
                                                                 : ((const float *)x->f->data[0])[i * ch + c];
                    out[n++] = clamp16(v);
                }
            }
            av_frame_unref(x->f);
        }
    }
    avcodec_flush_buffers(x->c);
    return n;
}

void lma_xma_reset(lma_xma *x) {
    avcodec_flush_buffers(x->c);
}

void lma_xma_close(lma_xma *x) {
    if (!x) return;
    avcodec_free_context(&x->c);
    av_packet_free(&x->p);
    av_frame_free(&x->f);
    av_free(x);
}
