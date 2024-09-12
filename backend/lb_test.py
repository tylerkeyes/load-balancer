import threading
import requests
import time

AVG_COUNT = 1


def query_lb(runs):
    for _ in range(runs):
        requests.get("http://localhost:4000")


def lb_test(runs, thread_cnt):
    start = time.perf_counter()
    threads = []
    for _ in range(thread_cnt):
        thread = threading.Thread(target=query_lb, args=(runs,))
        threads.append(thread)
        thread.start()

    for thread in threads:
        thread.join()
    # print(f"completed {runs * thread_cnt} requests in {time.perf_counter() - start}s")
    return time.perf_counter() - start


def compile_avg(api_calls, thread_cnt):
    avg = 0
    for _ in range(AVG_COUNT):
        duration = lb_test(api_calls, thread_cnt)
        avg += duration
    avg = avg / AVG_COUNT
    print(f"completed {api_calls * thread_cnt} requests in {avg}s")


def main():
    compile_avg(100, 4)


if __name__ == "__main__":
    main()
