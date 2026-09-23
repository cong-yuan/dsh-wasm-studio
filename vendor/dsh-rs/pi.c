#include <stdio.h>
#include <math.h>
#include <time.h>
#include <stdlib.h>

/* 方法一：Leibniz 级数 pi/4 = 1 - 1/3 + 1/5 - 1/7 + ... */
double pi_leibniz(long terms) {
    double sum = 0.0;
    for (long i = 0; i < terms; i++) {
        double term = 1.0 / (2 * i + 1);
        sum += (i % 2 == 0) ? term : -term;
    }
    return 4.0 * sum;
}

/* 方法二：蒙特卡洛法（随机撒点，落在单位圆内的比例 * 4） */
double pi_monte_carlo(long points) {
    srand(42);
    long inside = 0;
    for (long i = 0; i < points; i++) {
        double x = (double)rand() / RAND_MAX;
        double y = (double)rand() / RAND_MAX;
        if (x * x + y * y <= 1.0)
            inside++;
    }
    return 4.0 * (double)inside / points;
}

/* 方法三：BBP 公式（十六进制直接计算第 n 位，这里用十进制求和） */
double pi_bbp(long terms) {
    double sum = 0.0;
    for (long k = 0; k < terms; k++) {
        double p = pow(16.0, -k);
        double term = 4.0 / (8 * k + 1)
                    - 2.0 / (8 * k + 4)
                    - 1.0 / (8 * k + 5)
                    - 1.0 / (8 * k + 6);
        sum += p * term;
    }
    return sum;
}

int main(void) {
    const long N = 1000000;

    clock_t start, end;

    start = clock();
    double pi1 = pi_leibniz(N);
    end = clock();
    printf("Leibniz 级数 (%-8ld 项): pi = %.15f  误差 = %.2e  耗时 = %.3fs\n",
           N, pi1, fabs(pi1 - M_PI), (double)(end - start) / CLOCKS_PER_SEC);

    const long P = 1000000;
    start = clock();
    double pi2 = pi_monte_carlo(P);
    end = clock();
    printf("Monte Carlo (%-8ld 点):  pi = %.15f  误差 = %.2e  耗时 = %.3fs\n",
           P, pi2, fabs(pi2 - M_PI), (double)(end - start) / CLOCKS_PER_SEC);

    const long K = 100;
    start = clock();
    double pi3 = pi_bbp(K);
    end = clock();
    printf("BBP 公式    (%-8ld 项): pi = %.15f  误差 = %.2e  耗时 = %.3fs\n",
           K, pi3, fabs(pi3 - M_PI), (double)(end - start) / CLOCKS_PER_SEC);

    printf("\n参考值 M_PI = %.15f\n", M_PI);
    return 0;
}
