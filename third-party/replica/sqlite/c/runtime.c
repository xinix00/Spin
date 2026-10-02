/* Freestanding hulpfuncties: geen heap, bestanden, klok of systeembibliotheek.
 * memcpy/memmove/memset/memcmp en softfloat komen van Rust compiler_builtins. */
#include <stdint.h>
#include <string.h>
#include <math.h>

size_t strlen(const char *s) {size_t n=0;while(s[n])++n;return n;}
int strcmp(const char *a,const char *b) {
 while(*a && *a==*b){++a;++b;}
 return (unsigned char)*a-(unsigned char)*b;
}
int strncmp(const char *a,const char *b,size_t n) {
 for(size_t i=0;i<n;++i){
  int d=(unsigned char)a[i]-(unsigned char)b[i];
  if(d || !a[i])return d;
 }
 return 0;
}
char *strchr(const char *s,int c) {
 unsigned char byte=(unsigned char)c;
 do {if((unsigned char)*s==byte)return (char *)s;}while(*s++);
 return 0;
}
char *strrchr(const char *s,int c) {
 char *last=0;unsigned char byte=(unsigned char)c;
 do {if((unsigned char)*s==byte)last=(char *)s;}while(*s++);
 return last;
}
size_t strcspn(const char *s,const char *reject) {
 size_t n=0;while(s[n] && !strchr(reject,(unsigned char)s[n]))++n;return n;
}
size_t strspn(const char *s,const char *accept) {
 size_t n=0;while(s[n] && strchr(accept,(unsigned char)s[n]))++n;return n;
}
void *memchr(const void *s,int c,size_t n) {
 const unsigned char *p=s;
 for(size_t i=0;i<n;++i)if(p[i]==(unsigned char)c)return (void *)(p+i);
 return 0;
}
double fabs(double d) {
 union {double d;uint64_t u;} v={.d=d};
 v.u &= UINT64_C(0x7fffffffffffffff);
 return v.d;
}
