#!/bin/sh
DIR=/mnt/SDCARD/RetroArch
CONTENT=$2
case "$CONTENT" in
    *.[mM]3[uU])
        if [ -d "$CONTENT" ]; then
            BASENAME=${CONTENT##*/}
            if [ -f "$CONTENT/$BASENAME" ]; then
                CONTENT=$CONTENT/$BASENAME
            else
                FOUND=
                for CANDIDATE in "$CONTENT"/*; do
                    case "$CANDIDATE" in
                        *.[mM]3[uU])
                            [ -f "$CANDIDATE" ] || continue
                            [ -z "$FOUND" ] || { echo "Ambiguous playlist folder" >&2; exit 1; }
                            FOUND=$CANDIDATE
                            ;;
                    esac
                done
                [ -n "$FOUND" ] || { echo "Missing playlist" >&2; exit 1; }
                INNER=${FOUND##*/}
                [ "${INNER%.*}" = "${BASENAME%.*}" ] || { echo "Playlist save identity differs" >&2; exit 1; }
                CONTENT=$FOUND
            fi
        fi
        ;;
esac
if [ "${3:-}" = "--choose-disc" ]; then
    CONFIG=/tmp/allium-disc-start.cfg
    [ ! -L "$CONFIG" ] || { echo "Unsafe disc-start config path" >&2; exit 1; }
    printf 'savestate_auto_load = "false"\ninitial_disk_change_enable = "true"\nconfig_save_on_exit = "false"\n' > "$CONFIG" || exit 1
    HOME=/mnt/SDCARD/RetroArch LD_PRELOAD=libpadsp.so exec "$DIR/retroarch" -v -L "$DIR/.retroarch/cores/$1_libretro.so" --appendconfig "$CONFIG" "$CONTENT"
fi
HOME=/mnt/SDCARD/RetroArch LD_PRELOAD=libpadsp.so exec "$DIR/retroarch" -v -L "$DIR/.retroarch/cores/$1_libretro.so" "$CONTENT"
