// ****
// Custom type defintions
///
// these are not generated from IDL but are useful for better compatibility or utility
// ****

/// backwards compatible u128 deserializing data from rust <=1.76.0 when u/i128 was 8-byte aligned
/// https://solana.stackexchange.com/questions/7720/using-u128-without-sacrificing-alignment-8
#[derive(
    Default,
    PartialEq,
    AnchorSerialize,
    AnchorDeserialize,
    Serialize,
    Deserialize,
    Copy,
    Clone,
    bytemuck::Zeroable,
    bytemuck::Pod,
    Debug,
)]
#[repr(C)]
pub struct u128(pub [u8; 16]);

impl u128 {
    /// convert self into the std `u128` type
    pub fn as_u128(&self) -> std::primitive::u128 {
        std::primitive::u128::from_le_bytes(self.0)
    }
}

impl From<std::primitive::u128> for self::u128 {
    fn from(value: std::primitive::u128) -> Self {
        Self(value.to_le_bytes())
    }
}

/// backwards compatible i128 deserializing data from rust <=1.76.0 when u/i128 was 8-byte aligned
/// https://solana.stackexchange.com/questions/7720/using-u128-without-sacrificing-alignment-8
#[derive(
    Default,
    PartialEq,
    AnchorSerialize,
    AnchorDeserialize,
    Serialize,
    Deserialize,
    Copy,
    Clone,
    bytemuck::Zeroable,
    bytemuck::Pod,
    Debug,
)]
#[repr(C)]
pub struct i128(pub [u8; 16]);

impl i128 {
    /// convert self into the std `i128` type
    pub fn as_i128(&self) -> core::primitive::i128 {
        core::primitive::i128::from_le_bytes(self.0)
    }
}

impl From<core::primitive::i128> for i128 {
    fn from(value: core::primitive::i128) -> Self {
        Self(value.to_le_bytes())
    }
}

#[repr(transparent)]
#[derive(AnchorDeserialize, AnchorSerialize, Copy, Clone, PartialEq, Debug)]
pub struct Signature(pub [u8; 64]);

impl Default for Signature {
    fn default() -> Self {
        Self([0_u8; 64])
    }
}

impl serde::Serialize for Signature {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Signature {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = <&[u8]>::deserialize(d)?;
        s.try_into()
            .map(Signature)
            .map_err(serde::de::Error::custom)
    }
}

impl anchor_lang::Space for Signature {
    const INIT_SPACE: usize = 8 * 64;
}

/// wrapper around fixed array types used for padding with `Default` implementation
#[repr(transparent)]
#[derive(AnchorDeserialize, AnchorSerialize, Copy, Clone, PartialEq)]
pub struct Padding<const N: usize>([u8; N]);
impl<const N: usize> Default for Padding<N> {
    fn default() -> Self {
        Self([0u8; N])
    }
}

impl<const N: usize> std::fmt::Debug for Padding<N> {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // don't print anything for padding...
        Ok(())
    }
}

impl<const N: usize> anchor_lang::Space for Padding<N> {
    const INIT_SPACE: usize = 8 * N;
}

/// A fixed-size array wrapper for lengths serde's derives do not cover.
///
/// serde implements `Serialize` and `Deserialize` for `[T; N]` only up to
/// N = 32. A generated account that holds a longer array, such as a quote
/// buffer's level slots, would fail to compile the moment it entered the IDL.
/// The wrapper carries the array and serializes it as a sequence, which is what
/// serde would have done.
#[derive(AnchorSerialize, AnchorDeserialize, Copy, Clone, PartialEq, Debug)]
pub struct BigArray<T: Copy, const N: usize>(pub [T; N]);

impl<T: Copy + Default, const N: usize> Default for BigArray<T, N> {
    fn default() -> Self {
        Self([T::default(); N])
    }
}

impl<T: Copy, const N: usize> std::ops::Deref for BigArray<T, N> {
    type Target = [T; N];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Copy, const N: usize> std::ops::DerefMut for BigArray<T, N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T: Copy + serde::Serialize, const N: usize> serde::Serialize for BigArray<T, N> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter())
    }
}

impl<'de, T: Copy + Default + serde::Deserialize<'de>, const N: usize> serde::Deserialize<'de>
    for BigArray<T, N>
{
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let values: Vec<T> = <Vec<T> as serde::Deserialize>::deserialize(deserializer)?;
        if values.len() != N {
            return Err(serde::de::Error::invalid_length(values.len(), &"N elements"));
        }
        let mut out = [T::default(); N];
        out.copy_from_slice(&values);
        Ok(Self(out))
    }
}

impl<T: Copy + anchor_lang::Space, const N: usize> anchor_lang::Space for BigArray<T, N> {
    const INIT_SPACE: usize = T::INIT_SPACE * N;
}

/// [`BigArray`] specialized to bytes.
///
/// `BigArray<u8, N>` cannot satisfy anchor's `Space`. The `InitSpace` derive
/// inlines primitive sizes instead of implementing `Space` for `u8`, and
/// coherence forbids a local `u8` specialization next to the generic impl. A
/// byte region past serde's 32-element derive limit, such as a condition block
/// or a staging buffer, therefore gets its own wrapper with a plain byte count.
#[derive(AnchorSerialize, AnchorDeserialize, Copy, Clone, PartialEq, Debug)]
pub struct ByteArray<const N: usize>(pub [u8; N]);

impl<const N: usize> Default for ByteArray<N> {
    fn default() -> Self {
        Self([0u8; N])
    }
}

impl<const N: usize> std::ops::Deref for ByteArray<N> {
    type Target = [u8; N];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<const N: usize> std::ops::DerefMut for ByteArray<N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<const N: usize> serde::Serialize for ByteArray<N> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter())
    }
}

impl<'de, const N: usize> serde::Deserialize<'de> for ByteArray<N> {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let values: Vec<u8> = <Vec<u8> as serde::Deserialize>::deserialize(deserializer)?;
        if values.len() != N {
            return Err(serde::de::Error::invalid_length(values.len(), &"N elements"));
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&values);
        Ok(Self(out))
    }
}

impl<const N: usize> anchor_lang::Space for ByteArray<N> {
    const INIT_SPACE: usize = N;
}
