// ugrep's main(), renamed and given a C name, so the `koda` binary can run
// ugrep when it's started as `ugrep` (see lib.rs).
#define main koda_search_ugrep_cxx_main
#include "ugrep.cpp"
#undef main

extern "C" int koda_search_ugrep(int argc, const char **argv)
{
  return koda_search_ugrep_cxx_main(argc, argv);
}
