#ifndef REPLICA_MATH_H
#define REPLICA_MATH_H
/* SQLite's kern gebruikt alleen deze freestanding IEEE-operaties. */
#define isnan(x) __builtin_isnan(x)
#define isinf(x) __builtin_isinf(x)
#define signbit(x) __builtin_signbit(x)
#define INFINITY __builtin_inff()
double fabs(double);
#endif
