cd /mnt/share
ENC=
for d in /sys/class/video4linux/video*; do
  case "$(cat $d/name)" in stateful-encoder) ENC=/dev/$(basename $d);; esac
done
echo "encoder node: $ENC"
export RUST_LOG=info NO_COLOR=1
echo "### selftest dmabuf"; ./framelight selftest --device $ENC --coded FWHT --width 640 --height 360 --frames 30 --out st_dma.fwht --zero-copy; echo "exit $?"
echo "### selftest dmabuf 1280x720"; ./framelight selftest --device $ENC --coded FWHT --width 1280 --height 720 --frames 20 --out st_720.fwht --zero-copy; echo "exit $?"
echo "### selftest mmap"; ./framelight selftest --device $ENC --coded FWHT --width 640 --height 360 --frames 30 --out st_mmap.fwht; echo "exit $?"
echo "### probe synthetic"; ./framelight probe --synthetic --device $ENC --coded FWHT --width 640 --height 360 --fps 30 --seconds 2 --out probe.fwht --quiet --keyframe-every 0.5; echo "exit $?"
echo "### decode check"; ffmpeg -hide_banner -v error -f fwht -i st_dma.fwht -f null - 2>&1 | head -3; ffprobe -v error -show_entries stream=codec_name,width,height -count_frames -show_entries stream=nb_read_frames -of csv st_dma.fwht 2>&1 | head
