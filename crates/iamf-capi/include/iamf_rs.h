/* C API for the iamf-rs streaming IAMF decoder.
 *
 * Shaped after the iamf-tools iterative decoder API consumed by Chromium's
 * IamfAudioDecoder: create from a descriptor blob, push bitstream bytes
 * (whole or partial OBUs), pull decoded temporal units as interleaved
 * little-endian PCM.
 *
 * Thread safety: one decoder instance must be used from one thread at a
 * time; distinct instances are independent.
 */
#ifndef IAMF_RS_H_
#define IAMF_RS_H_

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct IamfrsDecoder iamfrs_decoder;

enum iamfrs_status {
  IAMFRS_OK = 0,
  IAMFRS_ERR_INVALID_ARG = -1,
  IAMFRS_ERR_UNSUPPORTED = -2,
  IAMFRS_ERR_CORRUPT_DATA = -3,
  IAMFRS_ERR_BUFFER_TOO_SMALL = -4,
  IAMFRS_ERR_NO_TEMPORAL_UNIT = -5,
  IAMFRS_ERR_INTERNAL = -6,
};

enum iamfrs_sample_type {
  IAMFRS_SAMPLE_INT16_LE = 1, /* matches iamf_tools OutputSampleType */
  IAMFRS_SAMPLE_INT32_LE = 2,
};

enum iamfrs_channel_ordering {
  IAMFRS_ORDERING_IAMF = 0,    /* IAMF rendering order */
  IAMFRS_ORDERING_ANDROID = 1, /* Android AudioFormat / WAVE order */
};

/* Decoder configuration, mirroring iamf_tools IamfDecoderFactory::Settings. */
typedef struct IamfrsSettings {
  /* IAMF sound-system numbering shared with iamf_tools OutputLayout:
   * 0=stereo(A) 1=5.1(B) 2=5.1.2(C) 3=5.1.4(D) 4=E 5=F 6=G 7=H(22.2)
   * 8=7.1(I) 9=7.1.4(J) 10=7.1.2 11=3.1.2 12=mono 13=9.1.6
   * 14=binaural (HRTF for elements with headphones_rendering_mode == 1,
   * stereo fallback otherwise, matching iamf-tools/obr). */
  int32_t output_layout;
  /* 0 = auto (from the stream's bit depth), 1 = s16le, 2 = s32le. */
  int32_t sample_type;
  /* Mix presentation to decode, or -1 for automatic selection (a mix
   * declaring the requested layout, else the first). */
  int64_t mix_presentation_id;
  /* enum iamfrs_channel_ordering. */
  int32_t channel_ordering;
  /* Nonzero disables trimming at stream start / end (for callers whose
   * demuxer trims via edts/elst). */
  uint8_t disable_trim_start;
  uint8_t disable_trim_end;
} iamfrs_settings;

int iamfrs_decoder_create_from_descriptors(const uint8_t *descriptors,
                                           size_t size,
                                           const iamfrs_settings *settings,
                                           iamfrs_decoder **out);

/* Push bitstream bytes; partial OBUs are buffered internally. */
int iamfrs_decoder_decode(iamfrs_decoder *decoder, const uint8_t *data,
                          size_t size);

/* 1 when a decoded temporal unit is ready, else 0. */
int iamfrs_decoder_is_temporal_unit_available(const iamfrs_decoder *decoder);

/* Pops one temporal unit as interleaved little-endian PCM. On
 * IAMFRS_ERR_BUFFER_TOO_SMALL, *bytes_written holds the required size and
 * the unit is retained for the next call (call with capacity 0 to query). */
int iamfrs_decoder_get_output_temporal_unit(iamfrs_decoder *decoder,
                                            uint8_t *buffer, size_t capacity,
                                            size_t *bytes_written);

int iamfrs_decoder_get_num_output_channels(const iamfrs_decoder *decoder,
                                           uint32_t *out);
int iamfrs_decoder_get_sample_rate(const iamfrs_decoder *decoder,
                                   uint32_t *out);
int iamfrs_decoder_get_frame_size(const iamfrs_decoder *decoder,
                                  uint32_t *out);

/* The mix presentation actually selected (iamf_tools GetOutputMix). */
int iamfrs_decoder_get_selected_mix_presentation_id(
    const iamfrs_decoder *decoder, uint32_t *out);

/* The layout actually rendered, in the sound-system numbering documented
 * on iamfrs_settings.output_layout (may differ from the request, e.g.
 * binaural falling back to stereo). */
int iamfrs_decoder_get_selected_layout(const iamfrs_decoder *decoder,
                                       uint32_t *out);

/* The resolved output sample type: 1 = s16le, 2 = s32le. */
int iamfrs_decoder_get_sample_type(const iamfrs_decoder *decoder,
                                   uint32_t *out);

/* Drops buffered audio and parameter state (seek/discontinuity). */
int iamfrs_decoder_reset(iamfrs_decoder *decoder);

/* Marks end of stream; remaining buffered units stay pullable. */
int iamfrs_decoder_signal_end_of_decoding(iamfrs_decoder *decoder);

void iamfrs_decoder_destroy(iamfrs_decoder *decoder);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* IAMF_RS_H_ */
