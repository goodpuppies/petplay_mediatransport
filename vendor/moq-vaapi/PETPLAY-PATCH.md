# PetPlay patch of moq-vaapi 0.2.1

Unmodified crates.io `moq-vaapi 0.2.1` except `src/encode.rs`: the H.264 SPS uses
`pic_order_cnt_type(2)` instead of `0`.

Why: Quest 3's Qualcomm hardware decoder (through WebCodecs in Quest Browser) held every
picture until the next one arrived for the type-0 stream, although the SPS already declared
`max_num_reorder_frames = 0` and `max_dec_frame_buffering = 1`. That cost one frame interval of
latency (18.7 ms receive-to-decoded at 72 fps, against 6.9 ms with the software decoder). With
type 2 the display order is derived from `frame_num`, so no reordering is possible by
construction. Every picture is a reference and `count` restarts at each IDR, so the decoder's
derived order count equals the encoder's `2 * count` in the picture parameters.

Drop this patch when upstream offers the choice, or if a consumer needs type 0.
