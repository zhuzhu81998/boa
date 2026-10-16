function addOne(x) {
    return x + 1;
}

const n = 32_767;
let count = 0;
for (let i = 0; i < n; i++) {
    for (let j = 0; j < n; j++) {
        count = addOne(count);
    }
}
console.log(count);