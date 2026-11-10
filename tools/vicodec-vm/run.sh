# Runs inside the VM (see run-vm.sh): the self-test and a short probe against vicodec.
cd /mnt/share
ENC=
for d in /sys/class/video4linux/video*; do
  case "$(cat $d/name)" in stateful-encoder) ENC=/dev/$(basename $d);; esac
done
echo "encoder node: $ENC"
export RUST_LOG=info NO_COLOR=1
echo "### selftest 640x360"; ./framelight selftest --device $ENC --coded FWHT --width 640 --height 360 --frames 30 --out st_360.fwht; echo "exit $?"
echo "### selftest 1280x720"; ./framelight selftest --device $ENC --coded FWHT --width 1280 --height 720 --frames 20 --out st_720.fwht; echo "exit $?"
echo "### probe synthetic"; ./framelight probe --synthetic --device $ENC --coded FWHT --width 640 --height 360 --fps 30 --seconds 2 --out probe.fwht --keyframe-every 0.5; echo "exit $?"
