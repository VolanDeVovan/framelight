# vicodec (+ v4l2-mem2mem, which the stock NixOS kernel does not build) as an
# out-of-tree module package for the given kernel.
{ stdenv, kernel }:
stdenv.mkDerivation {
  pname = "vicodec-oot";
  version = kernel.version;
  src = kernel.src;
  nativeBuildInputs = kernel.moduleBuildDependencies;
  dontConfigure = true;
  buildPhase = ''
    mkdir oot
    cp drivers/media/v4l2-core/v4l2-mem2mem.c oot/
    cp drivers/media/test-drivers/vicodec/*.c drivers/media/test-drivers/vicodec/*.h oot/
    cat > oot/Kbuild <<'KB'
    obj-m += v4l2-mem2mem.o vicodec.o
    vicodec-objs := codec-fwht.o codec-v4l2-fwht.o vicodec-core.o
    KB
    sed -i 's/^    //' oot/Kbuild
    make -C ${kernel.dev}/lib/modules/${kernel.modDirVersion}/build M=$PWD/oot modules
  '';
  installPhase = ''
    d=$out/lib/modules/${kernel.modDirVersion}/extra
    mkdir -p $d
    cp oot/*.ko $d/
  '';
}
