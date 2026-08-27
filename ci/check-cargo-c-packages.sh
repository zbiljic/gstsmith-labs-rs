#!/usr/bin/env bash

set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

stage_root="$(mktemp -d /tmp/gstsmith-labs-cargo-c.XXXXXX)"
cleanup() {
  rm -rf -- "${stage_root:?}"
}
handle_signal() {
  trap - EXIT
  cleanup
  exit 1
}
trap cleanup EXIT
trap handle_signal HUP INT TERM

prefix="$stage_root/prefix"
libdir="$prefix/lib"
plugin_dir="$libdir/gstreamer-1.0"
pc_dir="$libdir/pkgconfig"

# Cargo package | cargo-c library/.pc stem | GStreamer plugin | element names
packages=(
  "gst-plugin-nalsense|gstnalsense|nalsense|nalsenseactivity nalsensereplay"
)

case "$(uname -s)" in
  Linux)
    dynamic_suffix="so"
    ;;
  Darwin)
    dynamic_suffix="dylib"
    ;;
  *)
    fail "unsupported platform: cargo-c package validation supports Linux and macOS"
    ;;
esac

modules=()
plugin_names=()
for mapping in "${packages[@]}"; do
  IFS='|' read -r package stem plugin elements <<<"$mapping"
  cargo cinstall \
    --locked \
    --package "$package" \
    --prefix "$prefix" \
    --libdir "$libdir" \
    --pkgconfigdir "$pc_dir"
  modules+=("$stem")
  plugin_names+=("$plugin")
done

for mapping in "${packages[@]}"; do
  IFS='|' read -r package stem plugin elements <<<"$mapping"
  dynamic_library="$plugin_dir/lib${stem}.${dynamic_suffix}"
  static_library="$plugin_dir/lib${stem}.a"
  expected_pc="$pc_dir/${stem}.pc"
  test -f "$dynamic_library" ||
    fail "missing dynamic library for $package: $dynamic_library"
  test -f "$static_library" ||
    fail "missing static archive for $package: $static_library"
  test -f "$expected_pc" ||
    fail "missing pkg-config module for $package: $expected_pc"
done

shopt -s nullglob
dynamic_files=("$plugin_dir"/lib*."$dynamic_suffix")
static_files=("$plugin_dir"/lib*.a)
pc_files=("$pc_dir"/*.pc)
expected_count="${#packages[@]}"
test "${#dynamic_files[@]}" -eq "$expected_count" ||
  fail "expected exactly $expected_count dynamic plugin libraries, found ${#dynamic_files[@]}"
test "${#static_files[@]}" -eq "$expected_count" ||
  fail "expected exactly $expected_count static plugin archives, found ${#static_files[@]}"
test "${#pc_files[@]}" -eq "$expected_count" ||
  fail "expected exactly $expected_count pkg-config modules, found ${#pc_files[@]}"

unset PKG_CONFIG_LIBDIR
export PKG_CONFIG_PATH="$pc_dir"
pkg-config --validate "${modules[@]}"
for module in "${modules[@]}"; do
  module_pc_dir="$(pkg-config --variable=pcfiledir "$module")"
  module_libdir="$(pkg-config --variable=libdir "$module")"
  test "$module_pc_dir" = "$pc_dir" ||
    fail "$module reports pkg-config directory $module_pc_dir instead of $pc_dir"
  test "$module_libdir" = "$libdir" ||
    fail "$module reports libdir $module_libdir instead of $libdir"
  module_libs="$(pkg-config --libs "$module")"
  case " $module_libs " in
    *" -L$plugin_dir "*) ;;
    *) fail "$module omits staged plugin search path -L$plugin_dir: $module_libs" ;;
  esac
done
pkg-config --static --cflags --libs "${modules[@]}" >/dev/null

inspect_staged() {
  local target="$1"
  local expected_filename="$2"
  local output filename

  if ! output="$(gst-inspect-1.0 "$target" 2>&1)"; then
    printf '%s\n' "$output" >&2
    fail "gst-inspect-1.0 failed for $target"
  fi
  filename="$(awk '$1 == "Filename" { print $2; exit }' <<<"$output")"
  test "$filename" = "$expected_filename" ||
    fail "$target resolved to $filename instead of staged $expected_filename"
}

export GST_REGISTRY="$stage_root/dynamic-registry.bin"
unset GST_PLUGIN_PATH
unset GST_PLUGIN_SYSTEM_PATH
unset GST_PLUGIN_SYSTEM_PATH_1_0
export GST_PLUGIN_PATH_1_0="$plugin_dir"

for mapping in "${packages[@]}"; do
  IFS='|' read -r package stem plugin elements <<<"$mapping"
  expected_filename="$plugin_dir/lib${stem}.${dynamic_suffix}"
  inspect_staged "$plugin" "$expected_filename"
  for element in $elements; do
    inspect_staged "$element" "$expected_filename"
  done
done

dynamic_hold="$stage_root/dynamic-libraries"
mkdir "$dynamic_hold"
for dynamic_library in "${dynamic_files[@]}"; do
  mv "$dynamic_library" "$dynamic_hold/"
done

write_static_consumer() {
  local source="$1"
  local declarations="$2"
  local registrations="$3"
  local checks="$4"

  cat >"$source" <<EOF
#include <gst/gst.h>

$declarations

static gboolean
registry_has_plugin(GstRegistry *registry, const gchar *name)
{
  GstPlugin *plugin = gst_registry_find_plugin(registry, name);

  if (plugin == NULL) {
    g_printerr("static plugin not found: %s\\n", name);
    return FALSE;
  }

  gst_object_unref(plugin);
  return TRUE;
}

int
main(int argc, char **argv)
{
  GstRegistry *registry;

  gst_init(&argc, &argv);

$registrations

  registry = gst_registry_get();
  if ($checks) {
    return 1;
  }

  return 0;
}
EOF
}

compile_static_consumer() {
  local source="$1"
  local binary="$2"
  local module="$3"
  local -a static_flags
  read -r -a static_flags <<<"$(pkg-config --static --cflags --libs "$module")"
  cc "$source" -o "$binary" "${static_flags[@]}"
}

export GST_REGISTRY="$stage_root/static-registry.bin"
# Each Rust staticlib contains std and its upstream Rust dependencies, so
# validate independently packaged plugins with one consumer per archive.
for mapping in "${packages[@]}"; do
  IFS='|' read -r package stem plugin elements <<<"$mapping"
  static_source="$stage_root/static-${plugin}-consumer.c"
  static_binary="$stage_root/static-${plugin}-consumer"
  write_static_consumer "$static_source" \
    "GST_PLUGIN_STATIC_DECLARE($plugin);" \
    "  GST_PLUGIN_STATIC_REGISTER($plugin);" \
    "!registry_has_plugin(registry, \"$plugin\")"
  compile_static_consumer "$static_source" "$static_binary" "$stem"
  "$static_binary"
done

printf 'Validated dynamic plugins: %s\n' "${plugin_names[*]}"
printf 'Validated pkg-config modules: %s\n' "${modules[*]}"
printf 'Validated %s static archives with one static consumer per plugin\n' "$expected_count"
