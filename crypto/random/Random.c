/* vim: set expandtab ts=4 sw=4: */
/*
 * You may redistribute this program and/or modify it under the terms of
 * the GNU General Public License as published by the Free Software Foundation,
 * either version 3 of the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */
#include "crypto/random/Random.h"
#include "crypto/random/seed/RandomSeed.h"
#include "crypto/random/seed/SystemRandomSeed.h"
#include "memory/Allocator.h"
#include "util/Bits.h"
#include "util/Assert.h"
#include "util/Base32.h"
#include "util/Identity.h"
#include "util/Endian.h"

#include <sodium/crypto_hash_sha256.h>
#include <sodium/crypto_stream_salsa20.h>

/**
 * cjdns random generator:
 * It is with great apprehension that I have decided to go forward with this random generator.
 * Sadly there doesn't exist any plain-and-simple random generation library for C without
 * bundling libevent, openssl or some other megalyth.
 *
 * Additionally most random generators use a feedback loop which is difficult to validate as
 * it has a period which is not immedietly obvious by looking at it. Additionally, this
 * feedback loop design leads to issues like:
 * http://www.openssl.org/news/secadv_prng.txt
 *
 * How this random generator works:
 * 1. All available random sources such as dev/urandom and sysctl(RANDOM_UUID) are combined
 *    with a rolling SHA-512 hash, the result is placed in the Random_SeedGen union.
 *
 * 2. Random_SeedGen is SHA-256 hashed into Random.tempSeed
 *
 * 3. Random numbers are generated by running salsa20 with Random.tempSeed as the key, and
 *    Random.nonce 64 bit counter which is incremented each run, never reset, and assumed
 *    never to wrap.
 *
 * Adding entropy to the generator is as follows:
 * Random_addRandom() adds a sample of randomness by rotating and XORing it into
 * Random_SeedGen.collectedEntropy.
 * Every 256 calls to Random_addRandom(), Random_SeedGen is again hashed into Random.tempSeed.
 * Note that Random.nonce is *not* reset ever during the operation of the generator because
 * otherwise, 512 successive calls to Random_addRandom() with the same input would cause the
 * random generator to repeat.
 *
 *
 * State-compromize extension:
 * It is acknoliged that a compromize of the generator's internal state will result in the
 * attacker knowing every output which has been and will be generated or with the current
 * tempSeed. After a further 256 calls to Random_addRandom(), the generator should recover.
 *
 * While using a feedback loop with a one-way hash function to frustrate backtracking seems
 * enticing, it stands to reason that the only way a hash function can be one-way is by
 * destroying entropy, destruction of entropy in a feedback system could lead to an oscillation
 * effect when it becomes entropy starved. Though this issue does not seem to have been
 * exploited in other prngs, proving that it cannot be exploited is beyond my abilities and the
 * devil you know is better than the devil you don't.
 *
 *
 * Iterative Guessing:
 * This generator only introduces the entropy given by Random_addRandom() once every 256 calls.
 * Assuming each call introduces at least 1 bit of good entropy, iterative guessing requires
 * guessing a 256 bit value for each iteration.
 *
 *
 * Input based Attacks:
 * The generator is as conservitive as possible about the entropy provided in calls to
 * Random_addRandom(), valuing each at 1 bit of entropy. Since the number is rotated and XORd
 * into collectedEntropy, some calls with 0 bits of entropy can be smoothed over by other calls
 * with > 1 bit of entropy. If Random_addRandom() is called arbitrarily many times with 0 bits
 * of entropy, since the inputs are XORd into collectedEntropy the entropy level of
 * collectedEntropy will remain unchanged.
 *
 * Even if the attacker is able to gather information from the generator's output and craft
 * inputs to Random_addRandom() which *decrease* the entropy in collectedEntropy, this will not
 * decrease the performance of the generator itself because the 256 bit Random_SeedGen.seed
 * is seeded with the primary seed meterial (eg dev/urandom) and never altered for duration of
 * the generator's operation.
 */

/** How many bytes to buffer so requests for a small amount of random do not invoke salsa20. */
#define BUFFSIZE 128

/** The key material which is used to generate the temporary seed. */
union Random_SeedGen
{
    struct {
        /**
         * Read directly from the seed supplier (dev/urandom etc.),
         * same for the whole run of the generator.
         */
        uint64_t seed[4];

        /**
         * Initialized by the seed supplier
         * then XORd with the input given to Random_addRandom().
         */
        uint32_t collectedEntropy[8];
    } elements;

    /** Used to generate tempSeed. */
    uint64_t buff[8];
};

struct Random
{
    /** The random seed which is used to generate random numbers. */
    uint64_t tempSeed[4];

    /** Incremented every call to salsa20, never reset. */
    uint64_t nonce;

    /** buffer of random generated in the last rand cycle. */
    uint8_t buff[BUFFSIZE];

    /** the next number to read out of buff. */
    int nextByte;

    /** A counter which Random_addRandom() uses to rotate the random input. */
    int addRandomCounter;

    /** The seed generator for generating new temporary random seeds. */
    union Random_SeedGen* seedGen;

    /** The collector for getting the original permanent random seed from the operating system. */
    RandomSeed_t* seed;

    Identity
};

/**
 * Add a random number to the entropy pool.
 * 1 bit of entropy is extracted from each call to addRandom(), every 256 calls
 * this function will generate a new temporary seed using the permanent seed and
 * the collected entropy.
 *
 * Worst case scenario, Random_addRandom() is completely broken, the original
 * seed is still used and the nonce is never reset so the only loss is forward secrecy.
 */
void Random_addRandom(struct Random* rand, uint32_t randomNumber)
{
    Identity_check(rand);
    #define rotl(a,b) (((a) << (b)) | ((a) >> (31 - (b))))
    rand->seedGen->elements.collectedEntropy[rand->addRandomCounter % 8] ^=
        rotl(randomNumber, rand->addRandomCounter / 8);
    if (++rand->addRandomCounter >= 256) {
        crypto_hash_sha256((uint8_t*)rand->tempSeed,
                           (uint8_t*)rand->seedGen->buff,
                           sizeof(union Random_SeedGen));
        rand->addRandomCounter = 0;
    }
}

static void stir(struct Random* rand)
{
    uint64_t nonce = Endian_hostToLittleEndian64(rand->nonce);
    crypto_stream_salsa20_xor((uint8_t*)rand->buff,
                              (uint8_t*)rand->buff,
                              BUFFSIZE,
                              (uint8_t*)&nonce,
                              (uint8_t*)rand->tempSeed);
    rand->nonce++;
    rand->nextByte = 0;
}

static uintptr_t randomCopy(struct Random* rand, uint8_t* location, uint64_t count)
{
    uintptr_t num = (uintptr_t) count;
    if (num > (uintptr_t)(BUFFSIZE - rand->nextByte)) {
        num = (BUFFSIZE - rand->nextByte);
    }
    Bits_memcpy(location, &rand->buff[rand->nextByte], num);
    rand->nextByte += num;
    return num;
}

void Random_bytes(struct Random* rand, uint8_t* location, uint64_t count)
{
    Identity_check(rand);
    if (count > BUFFSIZE) {
        // big request, don't buffer it.
        crypto_stream_salsa20_xor((uint8_t*)location,
                                  (uint8_t*)location,
                                  count,
                                  (uint8_t*)&rand->nonce,
                                  (uint8_t*)rand->tempSeed);
        rand->nonce++;
        return;
    }

    for (;;) {
        uintptr_t sz = randomCopy(rand, location, count);
        location += sz;
        count -= sz;
        if (count == 0) {
            return;
        }
        stir(rand);
    }
}

void Random_base32(struct Random* rand, uint8_t* output, uint32_t length)
{
    Identity_check(rand);
    uint64_t index = 0;
    for (;;) {
        uint8_t bin[16];
        Random_bytes(rand, bin, 16);
        int ret = Base32_encode(&output[index], length - index, (uint8_t*)bin, 16);
        if (ret == Base32_TOO_BIG || index + ret == length) {
            break;
        }
        index += ret;
    }
    output[length - 1] = '\0';
}

struct Random* Random_newWithSeed(struct Allocator* alloc,
                                  struct Log* logger,
                                  RandomSeed_t* seed,
                                  struct Except* eh)
{
    union Random_SeedGen* seedGen = Allocator_calloc(alloc, sizeof(union Random_SeedGen), 1);

    if (RandomSeed_get(seed, seedGen->buff)) {
        Except_throw(eh, "Unable to initialize secure random number generator");
    }

    struct Random* rand = Allocator_calloc(alloc, sizeof(struct Random), 1);
    rand->seedGen = seedGen;
    rand->seed = seed;
    rand->nextByte = BUFFSIZE;

    Identity_set(rand);

    rand->addRandomCounter = 255;
    Random_addRandom(rand, 0);
    stir(rand);

    return rand;
}

struct Random* Random_new(struct Allocator* alloc, struct Log* logger, struct Except* eh)
{
    RandomSeed_t* rs = SystemRandomSeed_new(NULL, 0, logger, alloc);
    return Random_newWithSeed(alloc, logger, rs, eh);
}
