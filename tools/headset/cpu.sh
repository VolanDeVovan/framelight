# usage: cpu.sh SECONDS  -> CPU % of one core used by the framelight process
pid=$(pgrep -x framelight | head -1); [ -z "$pid" ] && { echo "no framelight"; exit 1; }
t() { awk '{print $14+$15}' /proc/$pid/stat; }
a=$(t); sleep $1; b=$(t)
echo "framelight pid $pid: $(awk -v a=$a -v b=$b -v s=$1 -v hz=$(getconf CLK_TCK) 'BEGIN{printf "%.2f", (b-a)/hz/s*100}')% of one core over ${1}s, threads $(ls /proc/$pid/task | wc -l), RSS $(awk '/VmRSS/{print $2}' /proc/$pid/status) kB"
