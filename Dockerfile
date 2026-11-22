# Shopping Sessions High-Performance Data Pipeline
# Target Architecture: linux/amd64 (Cascade Lake / AVX-512)
FROM gcc:15-bookworm AS build
WORKDIR /app
COPY src/ src/
RUN g++ -std=c++20 -O3 -flto -march=cascadelake -mtune=cascadelake \
    -Wa,-mbranches-within-32B-boundaries -fno-plt -static -pthread \
    -o /app/shopping-sessions src/main.cpp \
    && strip /app/shopping-sessions

FROM scratch
COPY --from=build /app/shopping-sessions /shopping-sessions
USER 1000:1000
ENV DATA_DIR=/data OUTPUT_DIR=/output
CMD ["/shopping-sessions"]
