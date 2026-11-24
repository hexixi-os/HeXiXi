#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <unistd.h>

int main(int argc, char **argv)
{
    const char *greeting = getenv("ELF_GREETING");
    const char *execfn = (const char *)getauxval(AT_EXECFN);
    if (argc != 2 || !greeting || strcmp(greeting, "Hello") ||
        !execfn || strcmp(execfn, argv[0]) || !getauxval(AT_BASE) ||
        !getauxval(AT_PHDR) || !getauxval(AT_ENTRY))
        return 1;
#ifdef __GLIBC__
    if (sbrk(4096) == (void *)-1 || sbrk(-4096) == (void *)-1)
        return 2;
#endif
    return printf("%s, %s!\n", greeting, argv[1]) < 0;
}
